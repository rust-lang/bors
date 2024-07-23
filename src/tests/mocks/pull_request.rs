use crate::github::GithubRepoName;
use octocrab::models::LabelId;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use url::Url;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

use super::{
    comment::{Comment, GitHubComment},
    default_repo_name, dynamic_mock_req,
    repository::GitHubRepository,
    user::GitHubUser,
    Repo, User,
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
    let prs = repo.lock().pull_requests.clone();
    for &pr_number in prs.keys() {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{repo_name}/pulls/{pr_number}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(GitHubPullRequest::new(pr_number)),
            )
            .mount(mock_server)
            .await;

        mock_pr_comments(
            repo_name.clone(),
            pr_number,
            comments_tx.clone(),
            mock_server,
        )
        .await;
        mock_pr_labels(repo.clone(), repo_name.clone(), pr_number, mock_server).await;
    }
}

async fn mock_pr_comments(
    repo_name: GithubRepoName,
    pr_number: u64,
    comments_tx: Sender<Comment>,
    mock_server: &MockServer,
) {
    Mock::given(method("POST"))
        .and(path(format!(
            "/repos/{repo_name}/issues/{pr_number}/comments",
        )))
        .respond_with(move |req: &Request| {
            #[derive(Deserialize)]
            struct CommentCreatePayload {
                body: String,
            }

            let comment_payload: CommentCreatePayload = req.body_json().unwrap();
            let comment: Comment =
                Comment::new(repo_name.clone(), pr_number, &comment_payload.body)
                    .with_author(User::bors_bot());

            // We cannot use `tx.blocking_send()`, because this function is actually called
            // from within an async task, but it is not async, so we also cannot use
            // `tx.send()`.
            comments_tx.try_send(comment.clone()).unwrap();
            ResponseTemplate::new(201).set_body_json(GitHubComment::from(comment))
        })
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
            pr.added_labels.extend(data.labels.clone());

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
            pr.removed_labels.push(label_name.to_string());

            ResponseTemplate::new(200).set_body_json::<&[GitHubLabel]>(&[])
        },
        "DELETE",
        format!("/repos/{repo_name}/issues/{pr_number}/labels/(.*)"),
    )
    .mount(mock_server)
    .await;
}

#[derive(Serialize)]
struct GitHubPullRequest {
    url: String,
    id: u64,
    title: String,
    body: String,

    /// The pull request number.  Note that GitHub's REST API
    /// considers every pull-request an issue with the same number.
    number: u64,

    head: Box<GitHubHead>,
    base: Box<GitHubBase>,

    user: GitHubUser,
}

impl GitHubPullRequest {
    fn new(number: u64) -> Self {
        GitHubPullRequest {
            user: User::default().into(),
            url: "https://test.com".to_string(),
            id: number + 1000,
            title: format!("PR #{number}"),
            body: format!("Description of PR #{number}"),
            number,
            head: Box::new(GitHubHead {
                label: format!("pr-{number}"),
                ref_field: format!("pr-{number}"),
                sha: format!("pr-{number}-sha"),
            }),
            base: Box::new(GitHubBase {
                ref_field: "main".to_string(),
                sha: "main-sha".to_string(),
            }),
        }
    }
}

impl Default for GitHubPullRequest {
    fn default() -> Self {
        Self::new(default_pr_number())
    }
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
struct GitHubLabel {
    id: LabelId,
    node_id: String,
    url: Url,
    name: String,
    color: String,
    default: bool,
}

#[derive(Serialize)]
pub(super) struct GitHubPullRequestEventPayload {
    action: String,
    pull_request: GitHubPullRequest,
    changes: Option<GitHubPullRequestChanges>,
    repository: GitHubRepository,
}

impl GitHubPullRequestEventPayload {
    pub fn new(pr_number: u64, action: String, changes: Option<PullRequestChangeEvent>) -> Self {
        GitHubPullRequestEventPayload {
            action,
            pull_request: GitHubPullRequest::new(pr_number),
            changes: changes.map(Into::into),
            repository: default_repo_name().into(),
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
        GitHubPullRequestChanges {
            base: value.from_base_sha.map(|sha| GitHubPullRequestBaseChanges {
                sha: Some(PullRequestEventChangesFrom { from: sha }),
            }),
        }
    }
}

pub struct PullRequestChangeEvent {
    pub from_base_sha: Option<String>,
}
