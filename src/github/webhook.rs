//! This module handles parsing webhooks and generating [`BorsEvent`]s from them.
use std::fmt::Debug;

use axum::body::Bytes;
use axum::extract::FromRequest;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::{async_trait, RequestExt};
use hmac::{Hmac, Mac};
use octocrab::models::events::payload::{
    IssueCommentEventAction, IssueCommentEventPayload, PullRequestEventAction,
    PullRequestEventChangesFrom, PullRequestReviewCommentEventAction,
    PullRequestReviewCommentEventPayload,
};
use octocrab::models::pulls::{PullRequest, Review};
use octocrab::models::{workflows, App, Author, CheckRun, Repository, RunId};
use secrecy::{ExposeSecret, SecretString};
use sha2::Sha256;

use crate::bors::event::{
    BorsEvent, BorsGlobalEvent, BorsRepositoryEvent, CheckSuiteCompleted, PullRequestComment,
    PullRequestEdited, PullRequestPushed, WorkflowCompleted, WorkflowStarted,
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
        self.0.expose_secret().as_str()
    }
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
    action: PullRequestEventAction,
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
#[async_trait]
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
        PullRequestEventAction::Edited => {
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
        PullRequestEventAction::Synchronize => Ok(Some(BorsEvent::Repository(
            BorsRepositoryEvent::PullRequestCommitPushed(PullRequestPushed {
                pull_request: payload.pull_request.into(),
                repository: repository_name,
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
        "completed" => Some(BorsEvent::Repository(
            BorsRepositoryEvent::WorkflowCompleted(WorkflowCompleted {
                repository: repository_name,
                branch: payload.workflow_run.head_branch,
                commit_sha: CommitSha(payload.workflow_run.head_sha),
                run_id: RunId(payload.workflow_run.id.0),
                status: match payload.workflow_run.conclusion.unwrap_or_default().as_str() {
                    "success" => WorkflowStatus::Success,
                    _ => WorkflowStatus::Failure,
                },
            }),
        )),
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
    use axum::extract::FromRequest;
    use hyper::StatusCode;
    use tokio::sync::mpsc;

    use crate::bors::event::{BorsEvent, BorsGlobalEvent};
    use crate::github::server::{ServerState, ServerStateRef};
    use crate::github::webhook::GitHubWebhook;
    use crate::github::webhook::WebhookSecret;
    use crate::tests::io::load_test_file;
    use crate::tests::webhook::{create_webhook_request, TEST_WEBHOOK_SECRET};

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
    async fn issue_comment() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/issue-comment.json", "issue_comment").await,
            @r###"
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
                        },
                    ),
                ),
            ),
        )
        "###
        );
    }

    #[tokio::test]
    async fn pull_request_edited() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-edited.json", "pull_request").await,
            @r###"
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
        "###
        );
    }

    #[tokio::test]
    async fn pull_request_synchronized() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-synchronize.json", "pull_request").await,
            @r###"
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
                            },
                        },
                    ),
                ),
            ),
        )
        "###
        );
    }

    #[tokio::test]
    async fn pull_request_review() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-review.json", "pull_request_review").await,
            @r###"
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
                        },
                    ),
                ),
            ),
        )
        "###
        );
    }

    #[tokio::test]
    async fn pull_request_review_comment() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/pull-request-review-comment.json", "pull_request_review_comment").await,
            @r###"
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
                        },
                    ),
                ),
            ),
        )
        "###
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
            check_webhook("webhook/push.json", "push")
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
        let server_ref = ServerStateRef::new(ServerState::new(
            repository_tx,
            global_tx,
            WebhookSecret::new(TEST_WEBHOOK_SECRET.to_string()),
        ));
        GitHubWebhook::from_request(request, &server_ref).await
    }
}
