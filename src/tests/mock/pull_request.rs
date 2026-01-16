use super::{
    GitHubUser, User, comment::GitHubComment, dynamic_mock_req, repository::GitHubRepository,
};
use crate::bors::PullRequestStatus;
use crate::github::GithubRepoName;
use crate::tests::Repo;
use crate::tests::github::{CommentMsg, PullRequest};
use crate::tests::{Comment, GitHub};
use chrono::{DateTime, Utc};
use octocrab::models::LabelId;
use octocrab::models::pulls::MergeableState as OctocrabMergeableState;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use url::Url;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

pub async fn mock_pull_requests(
    repo: Arc<Mutex<Repo>>,
    github: Arc<Mutex<GitHub>>,
    mock_server: &MockServer,
) {
    mock_pr_list(repo.clone(), github.clone(), mock_server).await;
    mock_pr(repo.clone(), github.clone(), mock_server).await;
    mock_pr_create(repo.clone(), github, mock_server).await;
    mock_pr_comments(repo.clone(), mock_server).await;
    mock_pr_labels(repo, mock_server).await;
}

async fn mock_pr(repo: Arc<Mutex<Repo>>, github: Arc<Mutex<GitHub>>, mock_server: &MockServer) {
    let repo_name = repo.lock().full_name();
    dynamic_mock_req(
        move |_req: &Request, [pr_number]: [&str; 1]| {
            let pr_number: u64 = pr_number.parse().unwrap();
            let pull_request_error = repo.lock().pull_request_error;
            if pull_request_error {
                ResponseTemplate::new(500)
            } else if let Some(pr) = repo.lock().pulls().get(&pr_number) {
                ResponseTemplate::new(200)
                    .set_body_json(GitHubPullRequest::new(&github.lock(), pr.clone()))
            } else {
                ResponseTemplate::new(404)
            }
        },
        "GET",
        format!("^/repos/{repo_name}/pulls/([0-9]+)$"),
    )
    .mount(mock_server)
    .await;
}

async fn mock_pr_create(
    repo: Arc<Mutex<Repo>>,
    github: Arc<Mutex<GitHub>>,
    mock_server: &MockServer,
) {
    let repo_name = repo.lock().full_name();
    dynamic_mock_req(
        move |req: &Request, []: [&str; 0]| {
            let mut repo = repo.lock();

            #[derive(serde::Deserialize)]
            struct RequestData {
                title: String,
                head: String,
                base: String,
                body: String,
            }

            let data: RequestData = req.body_json::<RequestData>().unwrap();

            // We only support PRs from forks for now
            assert!(data.head.contains(":"));

            let (fork_owner, branch) = data.head.split_once(":").unwrap();
            assert_ne!(fork_owner, repo.full_name().owner());
            let fork = github
                .lock()
                .repos
                .get(&GithubRepoName::new(fork_owner, repo.full_name().name()))
                .expect("Fork not found")
                .clone();
            assert!(fork.lock().fork);
            let commit = fork
                .lock()
                .get_branch_by_name(branch)
                .expect("Fork PR source branch not found")
                .get_commit()
                .clone();

            let pr_author = github
                .lock()
                .get_user(fork_owner)
                .expect("PR author not found")
                .clone();
            let base_branch = repo
                .get_branch_by_name(&data.base)
                .expect("Base branch not found")
                .clone();
            let pr = repo.add_pr(pr_author);
            pr.title = data.title;
            pr.description = data.body;
            pr.reset_to_single_commit(commit);
            pr.base_branch = base_branch;
            ResponseTemplate::new(200)
                .set_body_json(GitHubPullRequest::new(&github.lock(), pr.clone()))
        },
        "POST",
        format!("^/repos/{repo_name}/pulls$"),
    )
    .mount(mock_server)
    .await;
}

async fn mock_pr_list(
    repo_clone: Arc<Mutex<Repo>>,
    github: Arc<Mutex<GitHub>>,
    mock_server: &MockServer,
) {
    let repo_name = repo_clone.lock().full_name();
    Mock::given(method("GET"))
        .and(path(format!("/repos/{repo_name}/pulls")))
        .respond_with(move |req: &Request| {
            let pull_request_error = repo_clone.lock().pull_request_error;
            if pull_request_error {
                ResponseTemplate::new(500)
            } else {
                let query: HashMap<Cow<str>, Cow<str>> = req.url.query_pairs().collect();
                let filter_statuses = query
                    .get("state")
                    .map(|s| match s.as_ref() {
                        "open" => vec![PullRequestStatus::Open, PullRequestStatus::Draft],
                        "closed" => vec![PullRequestStatus::Closed, PullRequestStatus::Merged],
                        _ => vec![],
                    })
                    .unwrap_or_default();
                let prs = repo_clone.lock().pulls().clone();

                ResponseTemplate::new(200).set_body_json(
                    prs.values()
                        .filter(|pr| {
                            if !filter_statuses.is_empty() {
                                filter_statuses.contains(&pr.status)
                            } else {
                                true
                            }
                        })
                        .map(|pr| {
                            let mut pr = GitHubPullRequest::new(&github.lock(), pr.clone());
                            // GitHub always returns unknown mergeable state from this endpoint
                            pr.mergeable_state = OctocrabMergeableState::Unknown;
                            pr
                        })
                        .collect::<Vec<_>>(),
                )
            }
        })
        .mount(mock_server)
        .await;
}

async fn mock_pr_comments(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    let repo_name = repo.lock().full_name();
    dynamic_mock_req(
        move |req: &Request, [pr_number]: [&str; 1]| {
            let pr_number: u64 = pr_number.parse().unwrap();

            #[derive(Deserialize)]
            struct CommentCreatePayload {
                body: String,
            }

            let comment_payload: CommentCreatePayload = req.body_json().unwrap();
            let mut repo = repo.lock();
            let repo_name = repo.full_name();
            let pr = repo.pulls_mut().get_mut(&pr_number).unwrap_or_else(|| {
                panic!("Received a comment for a non-existing PR {repo_name}/{pr_number}")
            });
            let (id, node_id) = pr.next_comment_ids();

            let comment = Comment::new((repo_name.clone(), pr_number), &comment_payload.body)
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

async fn mock_pr_labels(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    let repo_name = repo.lock().full_name();
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
            let Some(pr) = repo.pulls_mut().get_mut(&pr_number) else {
                return ResponseTemplate::new(404);
            };
            pr.add_labels(data.labels.clone());

            let labels: Vec<GitHubLabel> = pr
                .labels
                .iter()
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
            let Some(pr) = repo.pulls_mut().get_mut(&pr_number) else {
                return ResponseTemplate::new(404);
            };
            pr.remove_label(label_name);

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
    html_url: String,
}

impl GitHubPullRequest {
    pub fn new(github: &GitHub, pr: PullRequest) -> Self {
        let head_sha = pr.head_sha();
        let PullRequest {
            number,
            repo,
            head_repository,
            labels_added_by_bors: _,
            labels_removed_by_bors: _,
            comment_counter: _,
            commits: _,
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

        if let Some(head_repository) = &head_repository {
            assert_ne!(head_repository, &repo);
        }

        GitHubPullRequest {
            user: author.clone().into(),
            url: "https://test.com".to_string(),
            html_url: format!("https://github.com/{repo}/pull/{number}"),
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
                repo: head_repository
                    .map(|repo| GitHubRepository::from(github.get_repo(repo).lock().deref())),
            }),
            base: Box::new(GitHubBase {
                ref_field: base_branch.name().to_string(),
                sha: base_branch.sha(),
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
    repo: Option<GitHubRepository>,
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
        repo: &Repo,
        github: &GitHub,
        pull_request: PullRequest,
        action: &str,
        changes: Option<PullRequestChangeEvent>,
    ) -> Self {
        GitHubPullRequestEventPayload {
            action: action.to_string(),
            pull_request: GitHubPullRequest::new(github, pull_request),
            changes: changes.map(Into::into),
            repository: GitHubRepository::from(repo),
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
    pub after: String,
}

impl GitHubPushEventPayload {
    pub fn new(repo: &Repo, branch_name: &str, sha: &str) -> Self {
        GitHubPushEventPayload {
            repository: GitHubRepository::from(repo),
            ref_field: format!("refs/heads/{branch_name}"),
            after: sha.to_string(),
        }
    }
}
