use super::{
    Repo, User,
    comment::{Comment, GitHubComment},
    default_repo_name, dynamic_mock_req,
    repository::GitHubRepository,
    user::GitHubUser,
};
use crate::github::PullRequestNumber;
use crate::tests::Branch;
use crate::{bors::PullRequestStatus, github::GithubRepoName};
use chrono::{DateTime, Utc};
use octocrab::models::LabelId;
use octocrab::models::pulls::{MergeableState as OctocrabMergeableState, MergeableState};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::mpsc::{Receiver, Sender};
use url::Url;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

pub fn default_pr_number() -> u64 {
    1
}

/// Helper struct for uniquely identifying a pull request.
/// Used to reduce boilerplate in tests.
///
/// Can be created from:
/// - `()`, which uses the default repo and default PR number.
/// - A PR number, which uses the default repository.
/// - A tuple (<repo-name>, <PR number>).
#[derive(Clone, Debug)]
pub struct PrIdentifier {
    pub repo: GithubRepoName,
    pub number: u64,
}

impl Display for PrIdentifier {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let PrIdentifier { repo, number } = self;
        f.write_fmt(format_args!("{repo}#{number}"))
    }
}

impl Default for PrIdentifier {
    fn default() -> Self {
        Self {
            repo: default_repo_name(),
            number: default_pr_number(),
        }
    }
}

impl From<u64> for PrIdentifier {
    fn from(number: u64) -> Self {
        Self {
            repo: default_repo_name(),
            number,
        }
    }
}

impl From<PullRequestNumber> for PrIdentifier {
    fn from(value: PullRequestNumber) -> Self {
        value.0.into()
    }
}

impl From<(GithubRepoName, u64)> for PrIdentifier {
    fn from(value: (GithubRepoName, u64)) -> Self {
        Self {
            repo: value.0,
            number: value.1,
        }
    }
}

impl From<()> for PrIdentifier {
    fn from(_: ()) -> Self {
        Self {
            repo: default_repo_name(),
            number: default_pr_number(),
        }
    }
}

pub enum CommentMsg {
    Comment(Comment),
    Close,
}

#[derive(Clone, Debug)]
pub struct PullRequest {
    pub number: PullRequestNumber,
    pub repo: GithubRepoName,
    pub labels_added_by_bors: Vec<String>,
    pub labels_removed_by_bors: Vec<String>,
    pub comment_counter: u64,
    pub head_sha: String,
    pub author: User,
    pub base_branch: Branch,
    pub mergeable_state: MergeableState,
    pub status: PullRequestStatus,
    pub merged_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    pub assignees: Vec<User>,
    pub description: String,
    pub title: String,
    pub labels: Vec<String>,
    pub comment_queue_tx: Sender<CommentMsg>,
    pub comment_queue_rx: Arc<tokio::sync::Mutex<Receiver<CommentMsg>>>,
    pub comment_history: Vec<Comment>,
}

impl PullRequest {
    pub fn new(repo: GithubRepoName, number: u64, author: User) -> Self {
        // The size of the buffer is load-bearing, if we receive too many comments, the test harness
        // could deadlock.
        let (comment_queue_tx, comment_queue_rx) = tokio::sync::mpsc::channel(100);
        Self {
            number: PullRequestNumber(number),
            repo,
            labels_added_by_bors: Vec::new(),
            labels_removed_by_bors: Vec::new(),
            comment_counter: 0,
            head_sha: format!("pr-{number}-sha"),
            author,
            base_branch: Branch::default(),
            mergeable_state: MergeableState::Clean,
            status: PullRequestStatus::Open,
            merged_at: None,
            closed_at: None,
            assignees: Vec::new(),
            description: format!("Description of PR {number}"),
            title: format!("Title of PR {number}"),
            labels: Vec::new(),
            comment_queue_tx,
            comment_queue_rx: Arc::new(tokio::sync::Mutex::new(comment_queue_rx)),
            comment_history: Vec::new(),
        }
    }
}

/// Creates a default pull request with number set to
/// [default_pr_number].
impl Default for PullRequest {
    fn default() -> Self {
        Self::new(
            default_repo_name(),
            default_pr_number(),
            User::default_pr_author(),
        )
    }
}

impl PullRequest {
    pub fn id(&self) -> PrIdentifier {
        PrIdentifier {
            repo: self.repo.clone(),
            number: self.number.0,
        }
    }

    /// Return a numeric ID and a node ID for the next comment to be created.
    pub fn next_comment_ids(&mut self) -> (u64, String) {
        self.comment_counter += 1;
        (
            self.comment_counter,
            format!(
                "comment-{}_{}-{}",
                self.repo, self.number, self.comment_counter
            ),
        )
    }

    pub fn merge_pr(&mut self) {
        self.merged_at = Some(SystemTime::now().into());
        self.status = PullRequestStatus::Merged;
    }

    pub fn close_pr(&mut self) {
        self.closed_at = Some(SystemTime::now().into());
        self.status = PullRequestStatus::Closed;
    }

    pub fn open_pr(&mut self) {
        self.status = PullRequestStatus::Open;
        self.closed_at = None;
    }

    pub fn reopen_pr(&mut self) {
        self.closed_at = None;
        self.status = PullRequestStatus::Open;
    }

    pub fn ready_for_review(&mut self) {
        self.status = PullRequestStatus::Open;
    }

    pub fn convert_to_draft(&mut self) {
        self.status = PullRequestStatus::Draft;
    }

    pub fn add_comment_to_history(&mut self, comment: Comment) {
        self.comment_history.push(comment);
    }
}

pub async fn mock_pull_requests(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
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

    mock_pr_comments(repo.clone(), mock_server).await;
    mock_pr_labels(repo.clone(), repo_name.clone(), mock_server).await;
}

async fn mock_pr_comments(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
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
            let pr = repo.pull_requests.get_mut(&pr_number).unwrap_or_else(|| {
                panic!("Received a comment for a non-existing PR {repo_name_clone}/{pr_number}")
            });
            let (id, node_id) = pr.next_comment_ids();

            let comment = Comment::new((repo_name_clone.clone(), pr_number), &comment_payload.body)
                .with_author(User::bors_bot())
                .with_ids(id, node_id);

            // We cannot use `tx.blocking_send()`, because this function is actually called
            // from within an async task, but it is not async, so we also cannot use
            // `tx.send()`.
            pr.comment_queue_tx
                .try_send(CommentMsg::Comment(comment.clone()))
                .unwrap();
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
    mock_server: &MockServer,
) {
    let repo2 = repo.clone();
    // Add label(s)
    dynamic_mock_req(
        move |req: &Request, [pr_number]: [&str; 1]| {
            let pr_number: u64 = pr_number.parse().unwrap();

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
        },
        "POST",
        format!("^/repos/{repo_name}/issues/([0-9]+)/labels$"),
    )
    .mount(mock_server)
    .await;

    // Remove label(s)
    dynamic_mock_req(
        move |_req: &Request, [pr_number, label_name]: [&str; 2]| {
            let pr_number: u64 = pr_number.parse().unwrap();

            let mut repo = repo2.lock();
            let Some(pr) = repo.pull_requests.get_mut(&pr_number) else {
                return ResponseTemplate::new(404);
            };
            pr.labels_removed_by_bors.push(label_name.to_string());

            ResponseTemplate::new(200).set_body_json::<&[GitHubLabel]>(&[])
        },
        "DELETE",
        format!("/repos/{repo_name}/issues/([0-9]+)/labels/(.*)"),
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
            comment_queue_tx: _,
            comment_queue_rx: _,
            comment_history: _,
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
pub struct GitHubPullRequestEventPayload {
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
