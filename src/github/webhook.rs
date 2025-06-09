//! This module handles parsing webhooks and generating [`BorsEvent`]s from them.
use std::fmt::Debug;

use axum::RequestExt;
use axum::body::Bytes;
use axum::extract::FromRequest;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use hmac::{Hmac, Mac};
use octocrab::models::events::payload::{
    IssueCommentEventAction, IssueCommentEventPayload, PullRequestEventChangesFrom,
    PullRequestReviewCommentEventAction, PullRequestReviewCommentEventPayload,
};
use octocrab::models::pulls::{PullRequest, Review};
use octocrab::models::webhook_events::payload::PullRequestWebhookEventAction;
use octocrab::models::{App, Author, CheckRun, Repository, RunId, workflows};
use secrecy::{ExposeSecret, SecretString};
use sha2::Sha256;

use crate::bors::event::{
    BorsEvent, BorsGlobalEvent, BorsRepositoryEvent, CheckSuiteCompleted, PullRequestAssigned,
    PullRequestClosed, PullRequestComment, PullRequestConvertedToDraft, PullRequestEdited,
    PullRequestMerged, PullRequestOpened, PullRequestPushed, PullRequestReadyForReview,
    PullRequestReopened, PullRequestUnassigned, PushToBranch, WorkflowCompleted, WorkflowStarted,
};
use crate::database::{WorkflowStatus, WorkflowType};
use crate::github::server::ServerStateRef;
use crate::github::{CommitSha, GithubRepoName, PullRequestNumber};

/// Wrapper for a secret which is zeroed on drop and can be exposed only through the
/// [`WebhookSecret::expose`] method.
pub struct WebhookSecret(SecretString);

impl WebhookSecret {
    pub fn new(secret: String) -> Self {
        Self(secret.into())
    }

    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

#[derive(serde::Deserialize, Debug)]
pub struct WebhookPushToBranchEvent {
    repository: Repository,
    #[serde(rename = "ref")]
    ref_field: String,
}

/// This struct is used to extract the repository and user from a GitHub webhook event.
/// The wrapper exists because octocrab doesn't expose/parse the repository field.
#[derive(serde::Deserialize, Debug)]
pub struct WebhookRepository {
    repository: Repository,
}

#[derive(serde::Deserialize, Debug)]
pub struct WebhookWorkflowRun<'a> {
    action: &'a str,
    workflow_run: workflows::Run,
    repository: Repository,
}

#[derive(serde::Deserialize, Debug)]
pub struct CheckRunInner {
    #[serde(flatten)]
    check_run: CheckRun,
    name: String,
    check_suite: CheckSuiteInner,
    app: App,
}

#[derive(serde::Deserialize, Debug)]
pub struct WebhookCheckRun<'a> {
    action: &'a str,
    check_run: CheckRunInner,
    repository: Repository,
}

#[derive(serde::Deserialize, Debug)]
pub struct CheckSuiteInner {
    head_branch: String,
    head_sha: String,
}

#[derive(serde::Deserialize, Debug)]
pub struct WebhookCheckSuite<'a> {
    action: &'a str,
    check_suite: CheckSuiteInner,
    repository: Repository,
}

#[derive(Debug, serde::Deserialize)]
pub struct WebhookPullRequestReviewEvent<'a> {
    action: &'a str,
    pull_request: PullRequest,
    review: Review,
    repository: Repository,
    sender: Author,
}

/// Similar to PullRequestEvent from octocrab, but changes field also includes base sha.
/// https://docs.github.com/en/webhooks/webhook-events-and-payloads#pull_request
#[derive(Debug, serde::Deserialize)]
struct WebhookPullRequestEvent {
    action: PullRequestWebhookEventAction,
    pull_request: PullRequest,
    changes: Option<WebhookPullRequestChanges>,
    repository: Repository,
}

#[derive(Debug, serde::Deserialize)]
struct WebhookPullRequestChanges {
    base: Option<WebhookPullRequestBaseChanges>,
}

#[derive(Debug, serde::Deserialize)]
struct WebhookPullRequestBaseChanges {
    sha: Option<PullRequestEventChangesFrom>,
}

/// axum extractor for GitHub webhook events.
#[derive(Debug)]
pub struct GitHubWebhook(pub BorsEvent);

const REQUEST_BODY_LIMIT: usize = 10 * 1024 * 1024;

/// Extracts a webhook event from a HTTP request.
impl FromRequest<ServerStateRef> for GitHubWebhook {
    type Rejection = StatusCode;

    async fn from_request(
        request: axum::extract::Request,
        state: &ServerStateRef,
    ) -> Result<Self, Self::Rejection> {
        let (parts, body) = request.with_limited_body().into_parts();

        // Eagerly load body
        let body: Bytes = axum::body::to_bytes(body, REQUEST_BODY_LIMIT)
            .await
            .map_err(|error| {
                tracing::error!("Parsing webhook body failed: {error:?}");
                StatusCode::BAD_REQUEST
            })?;

        // Verify that the request is valid
        if !verify_gh_signature(&parts.headers, &body, state.get_webhook_secret()) {
            tracing::error!("Webhook request failed, could not authenticate webhook");
            return Err(StatusCode::BAD_REQUEST);
        }

        // Parse webhook content
        match parse_webhook_event(parts, &body) {
            Ok(Some(event)) => {
                tracing::trace!("Received webhook event {event:?}");
                Ok(GitHubWebhook(event))
            }
            Ok(None) => Err(StatusCode::OK),
            Err(error) => {
                tracing::error!("Cannot parse webhook event: {error:?}");
                Err(StatusCode::BAD_REQUEST)
            }
        }
    }
}

fn parse_webhook_event(request: Parts, body: &[u8]) -> anyhow::Result<Option<BorsEvent>> {
    let Some(event_type) = request.headers.get("x-github-event") else {
        return Err(anyhow::anyhow!("x-github-event header not found"));
    };

    tracing::trace!(
        "Webhook: event_type `{}`, payload\n{}",
        event_type.to_str().unwrap_or_default(),
        std::str::from_utf8(body).unwrap_or_default()
    );

    match event_type.as_bytes() {
        b"push" => parse_push_event(body),
        b"issue_comment" => parse_issue_comment_event(body),
        b"pull_request" => parse_pull_request_events(body),
        b"pull_request_review" => parse_pull_request_review_events(body),
        b"pull_request_review_comment" => parse_pull_request_review_comment_events(body),
        b"installation_repositories" | b"installation" => Ok(Some(BorsEvent::Global(
            BorsGlobalEvent::InstallationsChanged,
        ))),
        b"workflow_run" => parse_workflow_run_events(body),
        b"check_run" => parse_check_run_events(body),
        b"check_suite" => parse_check_suite_events(body),
        _ => {
            tracing::debug!("Ignoring unknown event type {:?}", event_type.to_str());
            Ok(None)
        }
    }
}

fn parse_push_event(body: &[u8]) -> anyhow::Result<Option<BorsEvent>> {
    let payload: WebhookPushToBranchEvent = serde_json::from_slice(body)?;
    let repository = parse_repository_name(&payload.repository)?;

    let branch = if let Some(branch) = payload.ref_field.strip_prefix("refs/heads/") {
        branch.to_string()
    } else {
        return Ok(None);
    };

    Ok(Some(BorsEvent::Repository(
        BorsRepositoryEvent::PushToBranch(PushToBranch { repository, branch }),
    )))
}

fn parse_issue_comment_event(body: &[u8]) -> anyhow::Result<Option<BorsEvent>> {
    let repository: WebhookRepository = serde_json::from_slice(body)?;
    let repository_name = parse_repository_name(&repository.repository)?;

    let event: IssueCommentEventPayload = serde_json::from_slice(body)?;
    if event.action == IssueCommentEventAction::Created {
        let comment = parse_pr_comment(repository_name, event)
            .map(BorsRepositoryEvent::Comment)
            .map(BorsEvent::Repository);
        Ok(comment)
    } else {
        Ok(None)
    }
}

fn parse_pull_request_events(body: &[u8]) -> anyhow::Result<Option<BorsEvent>> {
    let payload: WebhookPullRequestEvent = serde_json::from_slice(body)?;
    let repository_name = parse_repository_name(&payload.repository)?;
    match payload.action {
        PullRequestWebhookEventAction::Edited => {
            let Some(changes) = payload.changes else {
                return Err(anyhow::anyhow!(
                    "Edited pull request event should have `changes` field"
                ));
            };
            Ok(Some(BorsEvent::Repository(
                BorsRepositoryEvent::PullRequestEdited(PullRequestEdited {
                    repository: repository_name,
                    pull_request: payload.pull_request.into(),
                    from_base_sha: changes
                        .base
                        .and_then(|base| base.sha)
                        .map(|sha| CommitSha(sha.from)),
                }),
            )))
        }
        PullRequestWebhookEventAction::Synchronize => Ok(Some(BorsEvent::Repository(
            BorsRepositoryEvent::PullRequestCommitPushed(PullRequestPushed {
                pull_request: payload.pull_request.into(),
                repository: repository_name,
            }),
        ))),
        PullRequestWebhookEventAction::Opened => Ok(Some(BorsEvent::Repository(
            BorsRepositoryEvent::PullRequestOpened(PullRequestOpened {
                repository: repository_name,
                draft: payload.pull_request.draft.unwrap_or(false),
                pull_request: payload.pull_request.into(),
            }),
        ))),
        PullRequestWebhookEventAction::Closed => Ok(Some(BorsEvent::Repository({
            if payload.pull_request.merged_at.is_some() {
                BorsRepositoryEvent::PullRequestMerged(PullRequestMerged {
                    repository: repository_name,
                    pull_request: payload.pull_request.into(),
                })
            } else {
                BorsRepositoryEvent::PullRequestClosed(PullRequestClosed {
                    repository: repository_name,
                    pull_request: payload.pull_request.into(),
                })
            }
        }))),
        PullRequestWebhookEventAction::Reopened => Ok(Some(BorsEvent::Repository(
            BorsRepositoryEvent::PullRequestReopened(PullRequestReopened {
                repository: repository_name,
                pull_request: payload.pull_request.into(),
            }),
        ))),
        PullRequestWebhookEventAction::ConvertedToDraft => Ok(Some(BorsEvent::Repository(
            BorsRepositoryEvent::PullRequestConvertedToDraft(PullRequestConvertedToDraft {
                repository: repository_name,
                pull_request: payload.pull_request.into(),
            }),
        ))),
        PullRequestWebhookEventAction::ReadyForReview => Ok(Some(BorsEvent::Repository(
            BorsRepositoryEvent::PullRequestReadyForReview(PullRequestReadyForReview {
                repository: repository_name,
                pull_request: payload.pull_request.into(),
            }),
        ))),
        PullRequestWebhookEventAction::Assigned => Ok(Some(BorsEvent::Repository(
            BorsRepositoryEvent::PullRequestAssigned(PullRequestAssigned {
                repository: repository_name,
                pull_request: payload.pull_request.into(),
            }),
        ))),
        PullRequestWebhookEventAction::Unassigned => Ok(Some(BorsEvent::Repository(
            BorsRepositoryEvent::PullRequestUnassigned(PullRequestUnassigned {
                repository: repository_name,
                pull_request: payload.pull_request.into(),
            }),
        ))),
        _ => Ok(None),
    }
}

fn parse_pull_request_review_events(body: &[u8]) -> anyhow::Result<Option<BorsEvent>> {
    let payload: WebhookPullRequestReviewEvent = serde_json::from_slice(body)?;
    if payload.action == "submitted" {
        let comment = parse_comment_from_pr_review(payload)?;
        Ok(Some(BorsEvent::Repository(BorsRepositoryEvent::Comment(
            comment,
        ))))
    } else {
        Ok(None)
    }
}
fn parse_pull_request_review_comment_events(body: &[u8]) -> anyhow::Result<Option<BorsEvent>> {
    let repository: WebhookRepository = serde_json::from_slice(body)?;
    let repository_name = parse_repository_name(&repository.repository)?;

    let payload: PullRequestReviewCommentEventPayload = serde_json::from_slice(body)?;
    if payload.action == PullRequestReviewCommentEventAction::Created {
        let comment = parse_pr_review_comment(repository_name, payload);
        Ok(Some(BorsEvent::Repository(BorsRepositoryEvent::Comment(
            comment,
        ))))
    } else {
        Ok(None)
    }
}

fn parse_workflow_run_events(body: &[u8]) -> anyhow::Result<Option<BorsEvent>> {
    let payload: WebhookWorkflowRun = serde_json::from_slice(body)?;
    let repository_name = parse_repository_name(&payload.repository)?;
    let result = match payload.action {
        "requested" => Some(BorsEvent::Repository(BorsRepositoryEvent::WorkflowStarted(
            WorkflowStarted {
                repository: repository_name,
                name: payload.workflow_run.name,
                branch: payload.workflow_run.head_branch,
                commit_sha: CommitSha(payload.workflow_run.head_sha),
                run_id: RunId(payload.workflow_run.id.0),
                workflow_type: WorkflowType::Github,
                url: payload.workflow_run.html_url.into(),
            },
        ))),
        "completed" => {
            let running_time = if let (Some(started_at), Some(completed_at)) = (
                Some(payload.workflow_run.created_at),
                Some(payload.workflow_run.updated_at),
            ) {
                Some(completed_at - started_at)
            } else {
                None
            };
            Some(BorsEvent::Repository(
                BorsRepositoryEvent::WorkflowCompleted(WorkflowCompleted {
                    repository: repository_name,
                    branch: payload.workflow_run.head_branch,
                    commit_sha: CommitSha(payload.workflow_run.head_sha),
                    run_id: RunId(payload.workflow_run.id.0),
                    running_time,
                    status: match payload.workflow_run.conclusion.unwrap_or_default().as_str() {
                        "success" => WorkflowStatus::Success,
                        _ => WorkflowStatus::Failure,
                    },
                }),
            ))
        }
        _ => None,
    };
    Ok(result)
}

fn parse_check_run_events(body: &[u8]) -> anyhow::Result<Option<BorsEvent>> {
    let payload: WebhookCheckRun = serde_json::from_slice(body).unwrap();

    // We are only interested in check runs from external CI services.
    // These basically correspond to workflow runs from GHA.
    if payload.check_run.app.owner.login == "github" {
        return Ok(None);
    }

    let repository_name = parse_repository_name(&payload.repository)?;
    if payload.action == "created" {
        Ok(Some(BorsEvent::Repository(
            BorsRepositoryEvent::WorkflowStarted(WorkflowStarted {
                repository: repository_name,
                name: payload.check_run.name.to_string(),
                branch: payload.check_run.check_suite.head_branch,
                commit_sha: CommitSha(payload.check_run.check_suite.head_sha),
                run_id: RunId(payload.check_run.check_run.id.map(|v| v.0).unwrap_or(0)),
                workflow_type: WorkflowType::External,
                url: payload.check_run.check_run.html_url.unwrap_or_default(),
            }),
        )))
    } else {
        Ok(None)
    }
}
fn parse_check_suite_events(body: &[u8]) -> anyhow::Result<Option<BorsEvent>> {
    let payload: WebhookCheckSuite = serde_json::from_slice(body)?;
    let repository_name = parse_repository_name(&payload.repository)?;
    if payload.action == "completed" {
        Ok(Some(BorsEvent::Repository(
            BorsRepositoryEvent::CheckSuiteCompleted(CheckSuiteCompleted {
                repository: repository_name,
                branch: payload.check_suite.head_branch,
                commit_sha: CommitSha(payload.check_suite.head_sha),
            }),
        )))
    } else {
        Ok(None)
    }
}

fn parse_pr_review_comment(
    repo: GithubRepoName,
    payload: PullRequestReviewCommentEventPayload,
) -> PullRequestComment {
    let user = payload.comment.user.into();
    PullRequestComment {
        repository: repo,
        author: user,
        pr_number: PullRequestNumber(payload.pull_request.number),
        text: payload.comment.body.unwrap_or_default(),
        html_url: payload.comment.html_url.to_string(),
    }
}

fn parse_comment_from_pr_review(
    payload: WebhookPullRequestReviewEvent<'_>,
) -> anyhow::Result<PullRequestComment> {
    let repository_name = parse_repository_name(&payload.repository)?;
    let user = payload.sender.into();

    Ok(PullRequestComment {
        repository: repository_name,
        author: user,
        pr_number: PullRequestNumber(payload.pull_request.number),
        text: payload.review.body.unwrap_or_default(),
        html_url: payload.review.html_url.to_string(),
    })
}

fn parse_pr_comment(
    repo: GithubRepoName,
    payload: IssueCommentEventPayload,
) -> Option<PullRequestComment> {
    // We only care about pull request comments
    if payload.issue.pull_request.is_none() {
        tracing::debug!("Ignoring event {payload:?} because it does not belong to a pull request");
        return None;
    }

    Some(PullRequestComment {
        repository: repo,
        author: payload.comment.user.into(),
        text: payload.comment.body.unwrap_or_default(),
        pr_number: PullRequestNumber(payload.issue.number),
        html_url: payload.comment.html_url.to_string(),
    })
}

fn parse_repository_name(repository: &Repository) -> anyhow::Result<GithubRepoName> {
    let repo_name = &repository.name;
    let Some(repo_owner) = repository.owner.as_ref().map(|u| &u.login) else {
        return Err(anyhow::anyhow!("Owner for repo {repo_name} is missing"));
    };
    Ok(GithubRepoName::new(repo_owner, repo_name))
}

type HmacSha256 = Hmac<Sha256>;

/// Verifies that the request is properly signed by GitHub with SHA-256 and the passed `secret`.
fn verify_gh_signature(
    headers: &HeaderMap<HeaderValue>,
    body: &[u8],
    secret: &WebhookSecret,
) -> bool {
    let Some(signature) = headers.get("x-hub-signature-256").map(|v| v.as_bytes()) else {
        return false;
    };
    let Some(signature) = signature
        .get(b"sha256=".len()..)
        .and_then(|v| hex::decode(v).ok())
    else {
        return false;
    };

    let mut mac =
        HmacSha256::new_from_slice(secret.expose().as_bytes()).expect("Cannot create HMAC key");
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::extract::FromRequest;
    use hyper::StatusCode;
    use sqlx::PgPool;
    use tokio::sync::mpsc;

    use crate::PgDbClient;
    use crate::bors::event::{BorsEvent, BorsGlobalEvent};
    use crate::github::server::{ServerState, ServerStateRef};
    use crate::github::webhook::GitHubWebhook;
    use crate::github::webhook::WebhookSecret;
    use crate::tests::io::load_test_file;
    use crate::tests::webhook::{TEST_WEBHOOK_SECRET, create_webhook_request};

    #[tokio::test]
    async fn installation_suspend() {
        assert!(matches!(
            check_webhook("webhook/installation-suspend.json", "installation",).await,
            Ok(GitHubWebhook(BorsEvent::Global(
                BorsGlobalEvent::InstallationsChanged
            )))
        ));
    }

    #[tokio::test]
    async fn installation_unsuspend() {
        assert!(matches!(
            check_webhook("webhook/installation-unsuspend.json", "installation",).await,
            Ok(GitHubWebhook(BorsEvent::Global(
                BorsGlobalEvent::InstallationsChanged
            )))
        ));
    }

    #[tokio::test]
    async fn push_to_branch() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/push.json", "push").await,
            @r#"
            Ok(
                GitHubWebhook(
                    Repository(
                        PushToBranch(
                            PushToBranch {
                                repository: GithubRepoName {
                                    owner: "kobzol",
                                    name: "bors-kindergarten",
                                },
                                branch: "main",
                            },
                        ),
                    ),
                ),
            )
            "#
        );
    }

    #[tokio::test]
    async fn issue_comment() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/issue-comment.json", "issue_comment").await,
            @r#"
        Ok(
            GitHubWebhook(
                Repository(
                    Comment(
                        PullRequestComment {
                            repository: GithubRepoName {
                                owner: "kobzol",
                                name: "bors-kindergarten",
                            },
                            author: GithubUser {
                                id: UserId(
                                    4539057,
                                ),
                                username: "Kobzol",
                                html_url: Url {
                                    scheme: "https",
                                    cannot_be_a_base: false,
                                    username: "",
                                    password: None,
                                    host: Some(
                                        Domain(
                                            "github.com",
                                        ),
                                    ),
                                    port: None,
                                    path: "/Kobzol",
                                    query: None,
                                    fragment: None,
                                },
                            },
                            pr_number: PullRequestNumber(
                                5,
                            ),
                            text: "hello bors",
                            html_url: "https://github.com/Kobzol/bors-kindergarten/pull/5#issuecomment-1420770715",
                        },
                    ),
                ),
            ),
        )
        "#
        );
    }

    #[tokio::test]
    async fn pull_request_edited() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-edited.json", "pull_request").await,
            @r#"
        Ok(
            GitHubWebhook(
                Repository(
                    PullRequestEdited(
                        PullRequestEdited {
                            repository: GithubRepoName {
                                owner: "vohoanglong0107",
                                name: "test-bors",
                            },
                            pull_request: PullRequest {
                                number: PullRequestNumber(
                                    1,
                                ),
                                head_label: "vohoanglong0107:test",
                                head: Branch {
                                    name: "test",
                                    sha: CommitSha(
                                        "bedf96270622ff19b4711dd7df3f19f4be1cba93",
                                    ),
                                },
                                base: Branch {
                                    name: "testest",
                                    sha: CommitSha(
                                        "1f1ee58e3067678d3752dd5f6f3abb936325fbb8",
                                    ),
                                },
                                title: "Create test.txt",
                                mergeable_state: Unknown,
                                message: "",
                                author: GithubUser {
                                    id: UserId(
                                        78085736,
                                    ),
                                    username: "vohoanglong0107",
                                    html_url: Url {
                                        scheme: "https",
                                        cannot_be_a_base: false,
                                        username: "",
                                        password: None,
                                        host: Some(
                                            Domain(
                                                "github.com",
                                            ),
                                        ),
                                        port: None,
                                        path: "/vohoanglong0107",
                                        query: None,
                                        fragment: None,
                                    },
                                },
                                assignees: [],
                                status: Open,
                            },
                            from_base_sha: Some(
                                CommitSha(
                                    "1f1ee58e3067678d3752dd5f6f3abb936325fbb8",
                                ),
                            ),
                        },
                    ),
                ),
            ),
        )
        "#
        );
    }

    #[tokio::test]
    async fn pull_request_synchronized() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-synchronize.json", "pull_request").await,
            @r#"
        Ok(
            GitHubWebhook(
                Repository(
                    PullRequestCommitPushed(
                        PullRequestPushed {
                            repository: GithubRepoName {
                                owner: "vohoanglong0107",
                                name: "test-bors",
                            },
                            pull_request: PullRequest {
                                number: PullRequestNumber(
                                    1,
                                ),
                                head_label: "vohoanglong0107:test",
                                head: Branch {
                                    name: "test",
                                    sha: CommitSha(
                                        "bedf96270622ff19b4711dd7df3f19f4be1cba93",
                                    ),
                                },
                                base: Branch {
                                    name: "main",
                                    sha: CommitSha(
                                        "1f1ee58e3067678d3752dd5f6f3abb936325fbb8",
                                    ),
                                },
                                title: "Create test.txt",
                                mergeable_state: Unknown,
                                message: "",
                                author: GithubUser {
                                    id: UserId(
                                        78085736,
                                    ),
                                    username: "vohoanglong0107",
                                    html_url: Url {
                                        scheme: "https",
                                        cannot_be_a_base: false,
                                        username: "",
                                        password: None,
                                        host: Some(
                                            Domain(
                                                "github.com",
                                            ),
                                        ),
                                        port: None,
                                        path: "/vohoanglong0107",
                                        query: None,
                                        fragment: None,
                                    },
                                },
                                assignees: [],
                                status: Open,
                            },
                        },
                    ),
                ),
            ),
        )
        "#
        );
    }

    #[tokio::test]
    async fn pull_request_review() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-review.json", "pull_request_review").await,
            @r#"
        Ok(
            GitHubWebhook(
                Repository(
                    Comment(
                        PullRequestComment {
                            repository: GithubRepoName {
                                owner: "kobzol",
                                name: "bors-kindergarten",
                            },
                            author: GithubUser {
                                id: UserId(
                                    4539057,
                                ),
                                username: "Kobzol",
                                html_url: Url {
                                    scheme: "https",
                                    cannot_be_a_base: false,
                                    username: "",
                                    password: None,
                                    host: Some(
                                        Domain(
                                            "github.com",
                                        ),
                                    ),
                                    port: None,
                                    path: "/Kobzol",
                                    query: None,
                                    fragment: None,
                                },
                            },
                            pr_number: PullRequestNumber(
                                6,
                            ),
                            text: "review comment",
                            html_url: "https://github.com/Kobzol/bors-kindergarten/pull/6#pullrequestreview-1476702458",
                        },
                    ),
                ),
            ),
        )
        "#
        );
    }

    #[tokio::test]
    async fn pull_request_opened() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-opened.json", "pull_request").await,
            @r#"
        Ok(
            GitHubWebhook(
                Repository(
                    PullRequestOpened(
                        PullRequestOpened {
                            repository: GithubRepoName {
                                owner: "sakib25800",
                                name: "bors-test",
                            },
                            pull_request: PullRequest {
                                number: PullRequestNumber(
                                    2,
                                ),
                                head_label: "Sakib25800:new-added-branch",
                                head: Branch {
                                    name: "new-added-branch",
                                    sha: CommitSha(
                                        "5f935077074fb7e225332e672399306591db30dd",
                                    ),
                                },
                                base: Branch {
                                    name: "main",
                                    sha: CommitSha(
                                        "a0d1e8a8a457bcea7550865cae33caf3a06699a6",
                                    ),
                                },
                                title: "New added branch",
                                mergeable_state: Unknown,
                                message: "This is a newly added branch from a just-opened PR. ",
                                author: GithubUser {
                                    id: UserId(
                                        66968718,
                                    ),
                                    username: "Sakib25800",
                                    html_url: Url {
                                        scheme: "https",
                                        cannot_be_a_base: false,
                                        username: "",
                                        password: None,
                                        host: Some(
                                            Domain(
                                                "github.com",
                                            ),
                                        ),
                                        port: None,
                                        path: "/Sakib25800",
                                        query: None,
                                        fragment: None,
                                    },
                                },
                                assignees: [],
                                status: Open,
                            },
                            draft: false,
                        },
                    ),
                ),
            ),
        )
        "#
        );
    }

    #[tokio::test]
    async fn pull_request_closed() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-closed.json", "pull_request").await,
            @r#"
        Ok(
            GitHubWebhook(
                Repository(
                    PullRequestClosed(
                        PullRequestClosed {
                            repository: GithubRepoName {
                                owner: "geetanshjuneja",
                                name: "test-bors",
                            },
                            pull_request: PullRequest {
                                number: PullRequestNumber(
                                    3,
                                ),
                                head_label: "geetanshjuneja:readme",
                                head: Branch {
                                    name: "readme",
                                    sha: CommitSha(
                                        "cf36029b6811f9e73e0c2dd2d8e41e3168c0a21a",
                                    ),
                                },
                                base: Branch {
                                    name: "main",
                                    sha: CommitSha(
                                        "ce5d683e04482608564a117910940fa2b563afde",
                                    ),
                                },
                                title: "bors test",
                                mergeable_state: Clean,
                                message: "",
                                author: GithubUser {
                                    id: UserId(
                                        72911296,
                                    ),
                                    username: "geetanshjuneja",
                                    html_url: Url {
                                        scheme: "https",
                                        cannot_be_a_base: false,
                                        username: "",
                                        password: None,
                                        host: Some(
                                            Domain(
                                                "github.com",
                                            ),
                                        ),
                                        port: None,
                                        path: "/geetanshjuneja",
                                        query: None,
                                        fragment: None,
                                    },
                                },
                                assignees: [],
                                status: Closed,
                            },
                        },
                    ),
                ),
            ),
        )
        "#
        );
    }

    #[tokio::test]
    async fn pull_request_merged() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-merged.json", "pull_request").await,
            @r#"
        Ok(
            GitHubWebhook(
                Repository(
                    PullRequestMerged(
                        PullRequestMerged {
                            repository: GithubRepoName {
                                owner: "geetanshjuneja",
                                name: "test-bors",
                            },
                            pull_request: PullRequest {
                                number: PullRequestNumber(
                                    5,
                                ),
                                head_label: "geetanshjuneja:test",
                                head: Branch {
                                    name: "test",
                                    sha: CommitSha(
                                        "4964158cdea899629716b4f0c0e2f5ab72e3cb4a",
                                    ),
                                },
                                base: Branch {
                                    name: "main",
                                    sha: CommitSha(
                                        "07eb9b80644df5476cb7d9bb23918d9e64b2dc35",
                                    ),
                                },
                                title: "test",
                                mergeable_state: Unknown,
                                message: "Creating draft pr",
                                author: GithubUser {
                                    id: UserId(
                                        72911296,
                                    ),
                                    username: "geetanshjuneja",
                                    html_url: Url {
                                        scheme: "https",
                                        cannot_be_a_base: false,
                                        username: "",
                                        password: None,
                                        host: Some(
                                            Domain(
                                                "github.com",
                                            ),
                                        ),
                                        port: None,
                                        path: "/geetanshjuneja",
                                        query: None,
                                        fragment: None,
                                    },
                                },
                                assignees: [],
                                status: Merged,
                            },
                        },
                    ),
                ),
            ),
        )
        "#
        );
    }

    #[tokio::test]
    async fn pull_request_reopened() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-reopened.json", "pull_request").await,
            @r#"
        Ok(
            GitHubWebhook(
                Repository(
                    PullRequestReopened(
                        PullRequestReopened {
                            repository: GithubRepoName {
                                owner: "geetanshjuneja",
                                name: "test-bors",
                            },
                            pull_request: PullRequest {
                                number: PullRequestNumber(
                                    3,
                                ),
                                head_label: "geetanshjuneja:readme",
                                head: Branch {
                                    name: "readme",
                                    sha: CommitSha(
                                        "cf36029b6811f9e73e0c2dd2d8e41e3168c0a21a",
                                    ),
                                },
                                base: Branch {
                                    name: "main",
                                    sha: CommitSha(
                                        "ce5d683e04482608564a117910940fa2b563afde",
                                    ),
                                },
                                title: "bors test",
                                mergeable_state: Unknown,
                                message: "",
                                author: GithubUser {
                                    id: UserId(
                                        72911296,
                                    ),
                                    username: "geetanshjuneja",
                                    html_url: Url {
                                        scheme: "https",
                                        cannot_be_a_base: false,
                                        username: "",
                                        password: None,
                                        host: Some(
                                            Domain(
                                                "github.com",
                                            ),
                                        ),
                                        port: None,
                                        path: "/geetanshjuneja",
                                        query: None,
                                        fragment: None,
                                    },
                                },
                                assignees: [],
                                status: Open,
                            },
                        },
                    ),
                ),
            ),
        )
        "#
        );
    }

    #[tokio::test]
    async fn pull_request_draft_opened() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-draft-opened.json", "pull_request").await,
            @r#"
        Ok(
            GitHubWebhook(
                Repository(
                    PullRequestOpened(
                        PullRequestOpened {
                            repository: GithubRepoName {
                                owner: "geetanshjuneja",
                                name: "test-bors",
                            },
                            pull_request: PullRequest {
                                number: PullRequestNumber(
                                    5,
                                ),
                                head_label: "geetanshjuneja:test",
                                head: Branch {
                                    name: "test",
                                    sha: CommitSha(
                                        "4964158cdea899629716b4f0c0e2f5ab72e3cb4a",
                                    ),
                                },
                                base: Branch {
                                    name: "main",
                                    sha: CommitSha(
                                        "07eb9b80644df5476cb7d9bb23918d9e64b2dc35",
                                    ),
                                },
                                title: "test",
                                mergeable_state: Unknown,
                                message: "Creating draft pr",
                                author: GithubUser {
                                    id: UserId(
                                        72911296,
                                    ),
                                    username: "geetanshjuneja",
                                    html_url: Url {
                                        scheme: "https",
                                        cannot_be_a_base: false,
                                        username: "",
                                        password: None,
                                        host: Some(
                                            Domain(
                                                "github.com",
                                            ),
                                        ),
                                        port: None,
                                        path: "/geetanshjuneja",
                                        query: None,
                                        fragment: None,
                                    },
                                },
                                assignees: [],
                                status: Draft,
                            },
                            draft: true,
                        },
                    ),
                ),
            ),
        )
        "#
        );
    }

    #[tokio::test]
    async fn pull_request_converted_to_draft() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-converted-to-draft.json", "pull_request").await,
            @r#"
        Ok(
            GitHubWebhook(
                Repository(
                    PullRequestConvertedToDraft(
                        PullRequestConvertedToDraft {
                            repository: GithubRepoName {
                                owner: "geetanshjuneja",
                                name: "test-bors",
                            },
                            pull_request: PullRequest {
                                number: PullRequestNumber(
                                    5,
                                ),
                                head_label: "geetanshjuneja:test",
                                head: Branch {
                                    name: "test",
                                    sha: CommitSha(
                                        "4964158cdea899629716b4f0c0e2f5ab72e3cb4a",
                                    ),
                                },
                                base: Branch {
                                    name: "main",
                                    sha: CommitSha(
                                        "07eb9b80644df5476cb7d9bb23918d9e64b2dc35",
                                    ),
                                },
                                title: "test",
                                mergeable_state: Clean,
                                message: "Creating draft pr",
                                author: GithubUser {
                                    id: UserId(
                                        72911296,
                                    ),
                                    username: "geetanshjuneja",
                                    html_url: Url {
                                        scheme: "https",
                                        cannot_be_a_base: false,
                                        username: "",
                                        password: None,
                                        host: Some(
                                            Domain(
                                                "github.com",
                                            ),
                                        ),
                                        port: None,
                                        path: "/geetanshjuneja",
                                        query: None,
                                        fragment: None,
                                    },
                                },
                                assignees: [],
                                status: Draft,
                            },
                        },
                    ),
                ),
            ),
        )
        "#
        );
    }

    #[tokio::test]
    async fn pull_request_ready_for_review() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-ready-for-review.json", "pull_request").await,
            @r#"
        Ok(
            GitHubWebhook(
                Repository(
                    PullRequestReadyForReview(
                        PullRequestReadyForReview {
                            repository: GithubRepoName {
                                owner: "geetanshjuneja",
                                name: "test-bors",
                            },
                            pull_request: PullRequest {
                                number: PullRequestNumber(
                                    5,
                                ),
                                head_label: "geetanshjuneja:test",
                                head: Branch {
                                    name: "test",
                                    sha: CommitSha(
                                        "4964158cdea899629716b4f0c0e2f5ab72e3cb4a",
                                    ),
                                },
                                base: Branch {
                                    name: "main",
                                    sha: CommitSha(
                                        "07eb9b80644df5476cb7d9bb23918d9e64b2dc35",
                                    ),
                                },
                                title: "test",
                                mergeable_state: Clean,
                                message: "Creating draft pr",
                                author: GithubUser {
                                    id: UserId(
                                        72911296,
                                    ),
                                    username: "geetanshjuneja",
                                    html_url: Url {
                                        scheme: "https",
                                        cannot_be_a_base: false,
                                        username: "",
                                        password: None,
                                        host: Some(
                                            Domain(
                                                "github.com",
                                            ),
                                        ),
                                        port: None,
                                        path: "/geetanshjuneja",
                                        query: None,
                                        fragment: None,
                                    },
                                },
                                assignees: [],
                                status: Open,
                            },
                        },
                    ),
                ),
            ),
        )
        "#
        );
    }

    #[tokio::test]
    async fn pull_request_review_comment() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-review-comment.json", "pull_request_review_comment").await,
            @r#"
        Ok(
            GitHubWebhook(
                Repository(
                    Comment(
                        PullRequestComment {
                            repository: GithubRepoName {
                                owner: "kobzol",
                                name: "bors-kindergarten",
                            },
                            author: GithubUser {
                                id: UserId(
                                    4539057,
                                ),
                                username: "Kobzol",
                                html_url: Url {
                                    scheme: "https",
                                    cannot_be_a_base: false,
                                    username: "",
                                    password: None,
                                    host: Some(
                                        Domain(
                                            "github.com",
                                        ),
                                    ),
                                    port: None,
                                    path: "/Kobzol",
                                    query: None,
                                    fragment: None,
                                },
                            },
                            pr_number: PullRequestNumber(
                                6,
                            ),
                            text: "Foo",
                            html_url: "https://github.com/Kobzol/bors-kindergarten/pull/6#discussion_r1227824551",
                        },
                    ),
                ),
            ),
        )
        "#
        );
    }

    #[tokio::test]
    async fn pull_request_assigned() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-assigned.json", "pull_request").await,
            @r#"
        Ok(
            GitHubWebhook(
                Repository(
                    PullRequestAssigned(
                        PullRequestAssigned {
                            repository: GithubRepoName {
                                owner: "sakib25800",
                                name: "bors-test-2",
                            },
                            pull_request: PullRequest {
                                number: PullRequestNumber(
                                    3,
                                ),
                                head_label: "Sakib25800:branch-2",
                                head: Branch {
                                    name: "branch-2",
                                    sha: CommitSha(
                                        "3824fd9a567e3c27dff0cd51427e7562a0eda8da",
                                    ),
                                },
                                base: Branch {
                                    name: "main",
                                    sha: CommitSha(
                                        "10636c870153028c034f7823ef8c3ad686e855f9",
                                    ),
                                },
                                title: "Branch 2",
                                mergeable_state: Clean,
                                message: "",
                                author: GithubUser {
                                    id: UserId(
                                        66968718,
                                    ),
                                    username: "Sakib25800",
                                    html_url: Url {
                                        scheme: "https",
                                        cannot_be_a_base: false,
                                        username: "",
                                        password: None,
                                        host: Some(
                                            Domain(
                                                "github.com",
                                            ),
                                        ),
                                        port: None,
                                        path: "/Sakib25800",
                                        query: None,
                                        fragment: None,
                                    },
                                },
                                assignees: [
                                    GithubUser {
                                        id: UserId(
                                            66968718,
                                        ),
                                        username: "Sakib25800",
                                        html_url: Url {
                                            scheme: "https",
                                            cannot_be_a_base: false,
                                            username: "",
                                            password: None,
                                            host: Some(
                                                Domain(
                                                    "github.com",
                                                ),
                                            ),
                                            port: None,
                                            path: "/Sakib25800",
                                            query: None,
                                            fragment: None,
                                        },
                                    },
                                ],
                                status: Open,
                            },
                        },
                    ),
                ),
            ),
        )
        "#
        );
    }

    #[tokio::test]
    async fn pull_request_unassigned() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-unassigned.json", "pull_request").await,
            @r#"
        Ok(
            GitHubWebhook(
                Repository(
                    PullRequestUnassigned(
                        PullRequestUnassigned {
                            repository: GithubRepoName {
                                owner: "sakib25800",
                                name: "bors-test-2",
                            },
                            pull_request: PullRequest {
                                number: PullRequestNumber(
                                    3,
                                ),
                                head_label: "Sakib25800:branch-2",
                                head: Branch {
                                    name: "branch-2",
                                    sha: CommitSha(
                                        "3824fd9a567e3c27dff0cd51427e7562a0eda8da",
                                    ),
                                },
                                base: Branch {
                                    name: "main",
                                    sha: CommitSha(
                                        "10636c870153028c034f7823ef8c3ad686e855f9",
                                    ),
                                },
                                title: "Branch 2",
                                mergeable_state: Clean,
                                message: "",
                                author: GithubUser {
                                    id: UserId(
                                        66968718,
                                    ),
                                    username: "Sakib25800",
                                    html_url: Url {
                                        scheme: "https",
                                        cannot_be_a_base: false,
                                        username: "",
                                        password: None,
                                        host: Some(
                                            Domain(
                                                "github.com",
                                            ),
                                        ),
                                        port: None,
                                        path: "/Sakib25800",
                                        query: None,
                                        fragment: None,
                                    },
                                },
                                assignees: [],
                                status: Open,
                            },
                        },
                    ),
                ),
            ),
        )
        "#
        );
    }

    #[tokio::test]
    async fn workflow_run_requested() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/workflow-run-requested.json", "workflow_run").await,
            @r###"
        Ok(
            GitHubWebhook(
                Repository(
                    WorkflowStarted(
                        WorkflowStarted {
                            repository: GithubRepoName {
                                owner: "kobzol",
                                name: "bors-kindergarten",
                            },
                            name: "Workflow 2",
                            branch: "automation/bors/try",
                            commit_sha: CommitSha(
                                "c9abcadf285659684c0975cead8bf982fa84e123",
                            ),
                            run_id: RunId(
                                4900979074,
                            ),
                            workflow_type: Github,
                            url: "https://github.com/Kobzol/bors-kindergarten/actions/runs/4900979074",
                        },
                    ),
                ),
            ),
        )
        "###
        );
    }

    #[tokio::test]
    async fn workflow_run_completed() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/workflow-run-completed.json", "workflow_run").await,
            @r###"
        Ok(
            GitHubWebhook(
                Repository(
                    WorkflowCompleted(
                        WorkflowCompleted {
                            repository: GithubRepoName {
                                owner: "kobzol",
                                name: "bors-kindergarten",
                            },
                            branch: "automation/bors/try",
                            commit_sha: CommitSha(
                                "c9abcadf285659684c0975cead8bf982fa84e123",
                            ),
                            run_id: RunId(
                                4900979072,
                            ),
                            status: Failure,
                            running_time: Some(
                                TimeDelta {
                                    secs: 13,
                                    nanos: 0,
                                },
                            ),
                        },
                    ),
                ),
            ),
        )
        "###
        );
    }

    #[tokio::test]
    async fn check_run_created_external() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/check-run-created-external.json", "check_run").await,
            @r###"
        Ok(
            GitHubWebhook(
                Repository(
                    WorkflowStarted(
                        WorkflowStarted {
                            repository: GithubRepoName {
                                owner: "kobzol",
                                name: "bors-kindergarten",
                            },
                            name: "check",
                            branch: "automation/bors/try-merge",
                            commit_sha: CommitSha(
                                "3d5258c8dd4fce72a4ea67387499fe69ea410928",
                            ),
                            run_id: RunId(
                                13293850093,
                            ),
                            workflow_type: External,
                            url: "https://github.com/Kobzol/bors-kindergarten/runs/13293850093",
                        },
                    ),
                ),
            ),
        )
        "###
        );
    }

    #[tokio::test]
    async fn check_run_created_gha() {
        assert!(matches!(
            check_webhook("webhook/check-run-created-gha.json", "check_run").await,
            Err(StatusCode::OK)
        ));
    }

    #[tokio::test]
    async fn unknown_event() {
        assert_eq!(
            check_webhook(
                "webhook/security-advisory-published.json",
                "security_advisory"
            )
            .await
            .unwrap_err(),
            StatusCode::OK
        );
    }

    async fn check_webhook(file: &str, event: &str) -> Result<GitHubWebhook, StatusCode> {
        let body = load_test_file(file);
        let request = create_webhook_request(event, &body);

        let (repository_tx, _) = mpsc::channel(1024);
        let (global_tx, _) = mpsc::channel(1024);
        let repos = HashMap::new();
        // We do not need an actual database;
        let db = Arc::new(PgDbClient::new(
            PgPool::connect_lazy("postgres://").unwrap(),
        ));
        let server_ref = ServerStateRef::new(ServerState::new(
            repository_tx,
            global_tx,
            WebhookSecret::new(TEST_WEBHOOK_SECRET.to_string()),
            repos,
            db,
        ));
        GitHubWebhook::from_request(request, &server_ref).await
    }
}
