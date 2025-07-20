use super::{
    Repo, User,
    comment::{Comment, GitHubComment},
    default_repo_name, dynamic_mock_req,
    repository::GitHubRepository,
    user::GitHubUser,
};
use crate::tests::mocks::repository::PullRequest;
use crate::{bors::PullRequestStatus, github::GithubRepoName};
use chrono::{DateTime, Utc};
use octocrab::models::LabelId;
use octocrab::models::pulls::MergeableState as OctocrabMergeableState;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use url::Url;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

pub fn default_pr_number() -> u64 {
    1
}

pub async fn mock_pull_requests(
    repo: Arc<Mutex<Repo>>,
    comments_tx: Sender<Comment>,
    mock_server: &MockServer,
) {
    let repo_name = repo.lock().name.clone();
    let repo_clone = repo.clone();

    Mock::given(method("GET"))
        .and(path(format!("/repos/{repo_name}/pulls")))
        .respond_with(move |_: &Request| {
            let pull_request_error = repo_clone.lock().pull_request_error;
            if pull_request_error {
                ResponseTemplate::new(500)
            } else {
                let prs = repo_clone.lock().pull_requests.clone();
                ResponseTemplate::new(200).set_body_json(
                    prs.values()
                        .into_iter()
                        .map(|pr| GitHubPullRequest::from(pr.clone()))
                        .filter(|pr| pr.closed_at.is_none())
                        .collect::<Vec<_>>(),
                )
            }
        })
        .mount(mock_server)
        .await;

    let repo_clone = repo.clone();
    dynamic_mock_req(
        move |_req: &Request, [pr_number]: [&str; 1]| {
            let pr_number: u64 = pr_number.parse().unwrap();
            let pull_request_error = repo_clone.lock().pull_request_error;
            if pull_request_error {
                ResponseTemplate::new(500)
            } else if let Some(pr) = repo_clone.lock().pull_requests.get(&pr_number) {
                ResponseTemplate::new(200).set_body_json(GitHubPullRequest::from(pr.clone()))
            } else {
                ResponseTemplate::new(404)
            }
        },
        "GET",
        format!("^/repos/{repo_name}/pulls/([0-9]+)$"),
    )
    .mount(mock_server)
    .await;

    mock_pr_comments(repo.clone(), comments_tx.clone(), mock_server).await;

    let prs = repo.lock().pull_requests.clone();
    for &pr_number in prs.keys() {
        mock_pr_labels(repo.clone(), repo_name.clone(), pr_number, mock_server).await;
    }
}

async fn mock_pr_comments(
    repo: Arc<Mutex<Repo>>,
    comments_tx: Sender<Comment>,
    mock_server: &MockServer,
) {
    let repo_name = repo.lock().name.clone();
    let repo_name_clone = repo_name.clone();
    dynamic_mock_req(
        move |req: &Request, [pr_number]: [&str; 1]| {
            let pr_number: u64 = pr_number.parse().unwrap();

            #[derive(Deserialize)]
            struct CommentCreatePayload {
                body: String,
            }

            let comment_payload: CommentCreatePayload = req.body_json().unwrap();
            let mut repo = repo.lock();
            let pr = repo.pull_requests.get_mut(&pr_number).unwrap();
            let comment_id = pr.next_comment_id();

            let comment = Comment::new(repo_name_clone.clone(), pr_number, &comment_payload.body)
                .with_author(User::bors_bot())
                .with_id(comment_id);

            // We cannot use `tx.blocking_send()`, because this function is actually called
            // from within an async task, but it is not async, so we also cannot use
            // `tx.send()`.
            comments_tx.try_send(comment.clone()).unwrap();
            ResponseTemplate::new(201).set_body_json(GitHubComment::from(comment))
        },
        "POST",
        format!("^/repos/{repo_name}/issues/([0-9]+)/comments$"),
    )
    .mount(mock_server)
    .await;
}

async fn mock_pr_labels(
    repo: Arc<Mutex<Repo>>,
    repo_name: GithubRepoName,
    pr_number: u64,
    mock_server: &MockServer,
) {
    let repo2 = repo.clone();
    // Add label(s)
    Mock::given(method("POST"))
        .and(path(format!(
            "/repos/{repo_name}/issues/{pr_number}/labels",
        )))
        .respond_with(move |req: &Request| {
            #[derive(serde::Deserialize)]
            struct CreateLabelsPayload {
                labels: Vec<String>,
            }

            let data: CreateLabelsPayload = req.body_json().unwrap();
            let mut repo = repo.lock();
            let Some(pr) = repo.pull_requests.get_mut(&pr_number) else {
                return ResponseTemplate::new(404);
            };
            pr.labels_added_by_bors.extend(data.labels.clone());

            let labels: Vec<GitHubLabel> = data
                .labels
                .into_iter()
                .map(|label| GitHubLabel {
                    id: 1.into(),
                    node_id: "".to_string(),
                    url: format!("https://github.com/labels/{label}")
                        .parse()
                        .unwrap(),
                    name: label.to_string(),
                    color: "blue".to_string(),
                    default: false,
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(labels)
        })
        .mount(mock_server)
        .await;

    // Remove label
    dynamic_mock_req(
        move |_req: &Request, [label_name]: [&str; 1]| {
            let mut repo = repo2.lock();
            let Some(pr) = repo.pull_requests.get_mut(&pr_number) else {
                return ResponseTemplate::new(404);
            };
            pr.labels_removed_by_bors.push(label_name.to_string());

            ResponseTemplate::new(200).set_body_json::<&[GitHubLabel]>(&[])
        },
        "DELETE",
        format!("/repos/{repo_name}/issues/{pr_number}/labels/(.*)"),
    )
    .mount(mock_server)
    .await;
}

#[derive(Serialize)]
pub struct GitHubPullRequest {
    url: String,
    id: u64,
    title: String,
    body: String,
    mergeable_state: OctocrabMergeableState,
    draft: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    merged_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    closed_at: Option<DateTime<Utc>>,

    /// The pull request number.  Note that GitHub's REST API
    /// considers every pull-request an issue with the same number.
    number: u64,

    head: Box<GitHubHead>,
    base: Box<GitHubBase>,

    user: GitHubUser,
    assignees: Vec<GitHubUser>,
    labels: Vec<GitHubLabel>,
}

impl From<PullRequest> for GitHubPullRequest {
    fn from(pr: PullRequest) -> Self {
        let PullRequest {
            number,
            repo: _,
            labels_added_by_bors: _,
            labels_removed_by_bors: _,
            comment_counter: _,
            head_sha,
            author,
            base_branch,
            mergeable_state,
            status,
            merged_at,
            closed_at,
            assignees,
            description,
            title,
            labels,
        } = pr;
        GitHubPullRequest {
            user: author.clone().into(),
            url: "https://test.com".to_string(),
            id: number.0 + 1000,
            title,
            body: description,
            mergeable_state,
            draft: status == PullRequestStatus::Draft,
            number: number.0,
            head: Box::new(GitHubHead {
                label: format!("pr-{number}"),
                ref_field: format!("pr-{number}"),
                sha: head_sha,
            }),
            base: Box::new(GitHubBase {
                ref_field: base_branch.get_name().to_string(),
                sha: base_branch.get_sha().to_string(),
            }),
            merged_at,
            closed_at,
            assignees: assignees.into_iter().map(Into::into).collect(),
            labels: labels
                .into_iter()
                .map(|label| GitHubLabel {
                    id: LabelId(1),
                    node_id: "".to_string(),
                    url: "https://test.com".to_string().parse().unwrap(),
                    name: label,
                    color: "".to_string(),
                    default: false,
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct GitHubLabel {
    id: LabelId,
    node_id: String,
    url: Url,
    name: String,
    color: String,
    default: bool,
}

#[derive(Serialize)]
struct GitHubHead {
    label: String,
    #[serde(rename = "ref")]
    ref_field: String,
    sha: String,
}

#[derive(Serialize)]
struct GitHubBase {
    #[serde(rename = "ref")]
    ref_field: String,
    sha: String,
}
#[derive(Serialize)]
pub(super) struct GitHubPullRequestEventPayload {
    action: String,
    pull_request: GitHubPullRequest,
    changes: Option<GitHubPullRequestChanges>,
    repository: GitHubRepository,
}

impl GitHubPullRequestEventPayload {
    pub fn new(
        pull_request: PullRequest,
        action: &str,
        changes: Option<PullRequestChangeEvent>,
    ) -> Self {
        let repository = pull_request.repo.clone();
        GitHubPullRequestEventPayload {
            action: action.to_string(),
            pull_request: pull_request.into(),
            changes: changes.map(Into::into),
            repository: repository.into(),
        }
    }
}

#[derive(Serialize)]
struct GitHubPullRequestChanges {
    base: Option<GitHubPullRequestBaseChanges>,
}

#[derive(Serialize)]
struct GitHubPullRequestBaseChanges {
    sha: Option<PullRequestEventChangesFrom>,
}

#[derive(Serialize)]
struct PullRequestEventChangesFrom {
    pub from: String,
}

impl From<PullRequestChangeEvent> for GitHubPullRequestChanges {
    fn from(value: PullRequestChangeEvent) -> Self {
        let base = if value.from_base_sha.is_some() {
            Some(GitHubPullRequestBaseChanges {
                sha: value
                    .from_base_sha
                    .map(|sha| PullRequestEventChangesFrom { from: sha }),
            })
        } else {
            None
        };

        GitHubPullRequestChanges { base }
    }
}

#[derive(Default)]
pub struct PullRequestChangeEvent {
    pub from_base_sha: Option<String>,
}

#[derive(Serialize)]
pub struct GitHubPushEventPayload {
    pub repository: GitHubRepository,
    #[serde(rename = "ref")]
    pub ref_field: String,
}

impl GitHubPushEventPayload {
    pub fn new(branch_name: &str) -> Self {
        GitHubPushEventPayload {
            repository: default_repo_name().into(),
            ref_field: format!("refs/heads/{branch_name}"),
        }
    }
}
