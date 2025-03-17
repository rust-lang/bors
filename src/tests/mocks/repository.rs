use std::collections::HashMap;
use std::sync::Arc;

use crate::bors::CheckSuiteStatus;
use base64::Engine;
use octocrab::models::repos::Object;
use octocrab::models::repos::Object::Commit;
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::mpsc::Sender;
use url::Url;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

use crate::github::{GithubRepoName, PullRequestNumber};
use crate::permissions::PermissionType;
use crate::tests::mocks::comment::Comment;
use crate::tests::mocks::permissions::Permissions;
use crate::tests::mocks::pull_request::mock_pull_requests;
use crate::tests::mocks::{default_pr_number, dynamic_mock_req, GitHubState, TestWorkflowStatus};

use super::user::{GitHubUser, User};

#[derive(Clone)]
pub struct PullRequest {
    pub number: PullRequestNumber,
    pub added_labels: Vec<String>,
    pub removed_labels: Vec<String>,
    pub comment_counter: u64,
    pub head_sha: String,
    pub author: User,
}

/// Creates a default pull request with number set to
/// [default_pr_number].
impl Default for PullRequest {
    fn default() -> Self {
        let number = default_pr_number();
        Self {
            number: PullRequestNumber(number),
            added_labels: Vec::new(),
            removed_labels: Vec::new(),
            comment_counter: 0,
            head_sha: format!("pr-{number}-sha"),
            author: User::default_pr_author(),
        }
    }
}

impl PullRequest {
    pub fn check_added_labels(&self, labels: &[&str]) {
        let added_labels = self
            .added_labels
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>();
        assert_eq!(&added_labels, labels);
    }

    pub fn check_removed_labels(&self, labels: &[&str]) {
        let removed_labels = self
            .removed_labels
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>();
        assert_eq!(&removed_labels, labels);
    }

    pub fn next_comment_id(&mut self) -> u64 {
        self.comment_counter += 1;
        self.comment_counter
    }
}

#[derive(Clone)]
pub struct Repo {
    pub name: GithubRepoName,
    pub permissions: Permissions,
    pub config: String,
    pub branches: Vec<Branch>,
    pub cancelled_workflows: Vec<u64>,
    pub workflow_cancel_error: bool,
    pub pull_requests: HashMap<u64, PullRequest>,
    // Cause pull request fetch to fail.
    pub pull_request_error: bool,
    pub pr_push_counter: u64,
}

impl Repo {
    pub fn new(name: GithubRepoName, permissions: Permissions, config: String) -> Self {
        Self {
            name,
            permissions,
            config,
            pull_requests: Default::default(),
            branches: vec![Branch::default()],
            cancelled_workflows: vec![],
            workflow_cancel_error: false,
            pull_request_error: false,
            pr_push_counter: 0,
        }
    }

    pub fn with_user_perms(mut self, user: User, permissions: &[PermissionType]) -> Self {
        self.permissions.users.insert(user, permissions.to_vec());
        self
    }

    pub fn with_pr(mut self, pull_request: PullRequest) -> Self {
        self.pull_requests
            .insert(pull_request.number.0, pull_request);
        self
    }

    pub fn get_pr(&self, pr: u64) -> &PullRequest {
        self.pull_requests.get(&pr).unwrap()
    }

    pub fn set_config(&mut self, config: &str) {
        self.config = config.to_string();
    }

    pub fn get_branch_by_name(&mut self, name: &str) -> Option<&mut Branch> {
        self.branches.iter_mut().find(|b| b.name == name)
    }

    pub fn get_branch_by_sha(&mut self, sha: &str) -> Option<&mut Branch> {
        self.branches.iter_mut().find(|b| b.sha == sha)
    }

    pub fn add_cancelled_workflow(&mut self, run_id: u64) {
        self.cancelled_workflows.push(run_id);
    }

    pub fn get_next_pr_push_count(&mut self) -> u64 {
        self.pr_push_counter += 1;
        self.pr_push_counter
    }
}

/// Represents the default repository for tests.
/// It uses a basic configuration that might be also encountered on a real repository.
///
/// It contains a single pull request by default, [PullRequest::default], and a single
/// branch called `main`.
impl Default for Repo {
    fn default() -> Self {
        let config = r#"
timeout = 3600

# Set labels on PR approvals
[labels]
approve = ["+approved"]
"#
        .to_string();

        let mut users = HashMap::default();
        users.insert(
            User::default(),
            vec![PermissionType::Try, PermissionType::Review],
        );
        users.insert(User::try_user(), vec![PermissionType::Try]);
        users.insert(
            User::reviewer(),
            vec![PermissionType::Try, PermissionType::Review],
        );

        Self::new(default_repo_name(), Permissions { users }, config)
            .with_pr(PullRequest::default())
    }
}

pub fn default_repo_name() -> GithubRepoName {
    GithubRepoName::new("rust-lang", "borstest")
}

#[derive(Clone)]
pub struct Branch {
    name: String,
    sha: String,
    commit_message: String,
    sha_history: Vec<String>,
    suite_statuses: Vec<CheckSuiteStatus>,
    merge_counter: u64,
    pub merge_conflict: bool,
}

impl Branch {
    pub fn new(name: &str, sha: &str) -> Self {
        Self {
            name: name.to_string(),
            sha: sha.to_string(),
            commit_message: format!("Commit {sha}"),
            sha_history: vec![],
            suite_statuses: vec![CheckSuiteStatus::Pending],
            merge_counter: 0,
            merge_conflict: false,
        }
    }

    pub fn get_name(&self) -> &str {
        &self.name
    }
    pub fn get_sha(&self) -> &str {
        &self.sha
    }
    pub fn get_suites(&self) -> &[CheckSuiteStatus] {
        &self.suite_statuses
    }

    /// Sets the expectation that this branch will receive `count` suites.
    pub fn expect_suites(&mut self, count: usize) {
        self.suite_statuses = vec![CheckSuiteStatus::Pending; count];
    }
    pub fn suite_finished(&mut self, status: TestWorkflowStatus) {
        for suite in self.suite_statuses.iter_mut() {
            if matches!(suite, CheckSuiteStatus::Pending) {
                *suite = match status {
                    TestWorkflowStatus::Success => CheckSuiteStatus::Success,
                    TestWorkflowStatus::Failure => CheckSuiteStatus::Failure,
                };
                return;
            }
        }
        panic!(
            "Received more suites than expected ({}) for branch {}",
            self.suite_statuses.len(),
            self.name
        );
    }
    pub fn reset_suites(&mut self) {
        for suite in self.suite_statuses.iter_mut() {
            *suite = CheckSuiteStatus::Pending;
        }
    }

    pub fn set_to_sha(&mut self, sha: &str) {
        self.sha_history.push(self.sha.clone());
        self.sha = sha.to_string();
    }

    pub fn get_sha_history(&self) -> Vec<String> {
        let mut shas = self.sha_history.clone();
        shas.push(self.sha.clone());
        shas
    }
}

impl Default for Branch {
    fn default() -> Self {
        Self::new(default_branch_name(), default_branch_sha())
    }
}

pub fn default_branch_name() -> &'static str {
    "main"
}

pub fn default_branch_sha() -> &'static str {
    "main-sha1"
}

pub async fn mock_repo_list(github: &GitHubState, mock_server: &MockServer) {
    let repos = GitHubRepositories {
        total_count: github.repos.len() as u64,
        repositories: github
            .repos
            .iter()
            .enumerate()
            .map(|(index, (_, repo))| {
                let repo = repo.lock();
                GitHubRepository {
                    id: index as u64,
                    owner: User::new(index as u64, repo.name.owner()).into(),
                    name: repo.name.name().to_string(),
                    url: format!("https://{}.foo", repo.name.name()).parse().unwrap(),
                }
            })
            .collect(),
    };

    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(repos))
        .mount(mock_server)
        .await;
}

pub async fn mock_repo(
    repo: Arc<Mutex<Repo>>,
    comments_tx: Sender<Comment>,
    mock_server: &MockServer,
) {
    mock_pull_requests(repo.clone(), comments_tx, mock_server).await;
    mock_branches(repo.clone(), mock_server).await;
    mock_cancel_workflow(repo.clone(), mock_server).await;
    mock_config(repo, mock_server).await;
}

async fn mock_branches(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    mock_get_branch(repo.clone(), mock_server).await;
    mock_create_branch(repo.clone(), mock_server).await;
    mock_update_branch(repo.clone(), mock_server).await;
    mock_merge_branch(repo.clone(), mock_server).await;
    mock_check_suites(repo, mock_server).await;
}

async fn mock_cancel_workflow(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    let repo_name = repo.lock().name.clone();
    dynamic_mock_req(
        move |_req: &Request, [run_id]: [&str; 1]| {
            let run_id: u64 = run_id.parse().unwrap();
            let mut repo = repo.lock();
            if repo.workflow_cancel_error {
                ResponseTemplate::new(500)
            } else {
                repo.add_cancelled_workflow(run_id);
                ResponseTemplate::new(200)
            }
        },
        "POST",
        format!("^/repos/{repo_name}/actions/runs/(.*)/cancel$"),
    )
    .mount(mock_server)
    .await;
}

async fn mock_get_branch(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    let repo_name = repo.lock().name.clone();
    dynamic_mock_req(
        move |_req: &Request, [branch_name]: [&str; 1]| {
            let mut repo = repo.lock();
            let Some(branch) = repo.get_branch_by_name(branch_name) else {
                return ResponseTemplate::new(404);
            };
            let branch = GitHubBranch {
                name: branch.name.clone(),
                commit: GitHubCommitObject {
                    sha: branch.sha.clone(),
                    url: format!("https://github.com/branch/{}-{}", branch.name, branch.sha)
                        .parse()
                        .unwrap(),
                },
                protected: false,
            };
            ResponseTemplate::new(200).set_body_json(branch)
        },
        "GET",
        format!("^/repos/{repo_name}/branches/(.*)$"),
    )
    .mount(mock_server)
    .await;
}

async fn mock_create_branch(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    let repo_name = repo.lock().name.clone();
    Mock::given(method("POST"))
        .and(path(format!("/repos/{repo_name}/git/refs")))
        .respond_with(move |request: &Request| {
            let mut repo = repo.lock();

            #[derive(serde::Deserialize)]
            struct SetRefRequest {
                r#ref: String,
                sha: String,
            }

            let data: SetRefRequest = request.body_json().unwrap();
            let branch_name = data
                .r#ref
                .strip_prefix("refs/heads/")
                .expect("Unexpected ref name");

            let sha = data.sha;
            match repo.get_branch_by_name(branch_name) {
                Some(branch) => {
                    panic!(
                        "Trying to create an already existing branch {}",
                        branch.name
                    );
                }
                None => {
                    // Create a new branch
                    repo.branches.push(Branch::new(branch_name, &sha));
                }
            }

            let url: Url = format!("https://github.com/branches/{branch_name}")
                .parse()
                .unwrap();
            let response = GitHubRef {
                ref_field: data.r#ref,
                node_id: repo.branches.len().to_string(),
                url: url.clone(),
                object: Commit { sha, url },
            };
            ResponseTemplate::new(200).set_body_json(response)
        })
        .mount(mock_server)
        .await;
}

async fn mock_update_branch(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    let repo_name = repo.lock().name.clone();
    dynamic_mock_req(
        move |req: &Request, [branch_name]: [&str; 1]| {
            let mut repo = repo.lock();

            #[derive(serde::Deserialize)]
            struct SetRefRequest {
                sha: String,
            }

            let data: SetRefRequest = req.body_json().unwrap();

            let sha = data.sha;
            match repo.get_branch_by_name(branch_name) {
                Some(branch) => {
                    // Update branch
                    branch.set_to_sha(&sha);
                }
                None => {
                    return ResponseTemplate::new(404);
                }
            }

            ResponseTemplate::new(200)
        },
        "PATCH",
        format!("^/repos/{repo_name}/git/refs/heads/(.*)$"),
    )
    .mount(mock_server)
    .await;
}

async fn mock_merge_branch(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!("/repos/{}/merges", repo.lock().name)))
        .respond_with(move |request: &Request| {
            let mut repo = repo.lock();

            #[derive(serde::Deserialize)]
            struct MergeRequest {
                base: String,
                head: String,
                commit_message: String,
            }

            let data: MergeRequest = request.body_json().unwrap();
            let head = repo.get_branch_by_name(&data.head);
            let head_sha = match head {
                None => {
                    // head is a SHA
                    data.head
                }
                Some(branch) => {
                    // head is a branch
                    branch.sha.clone()
                }
            };
            let Some(base_branch) = repo.get_branch_by_name(&data.base) else {
                return ResponseTemplate::new(404);
            };
            if base_branch.merge_conflict {
                // Conflict
                return ResponseTemplate::new(409);
            }

            let merge_sha = format!(
                "merge-{}-{head_sha}-{}",
                base_branch.sha, base_branch.merge_counter
            );
            base_branch.merge_counter += 1;
            base_branch.set_to_sha(&merge_sha);
            base_branch.commit_message = data.commit_message;

            #[derive(serde::Serialize)]
            struct MergeResponse {
                sha: String,
            }
            let response = MergeResponse { sha: merge_sha };
            ResponseTemplate::new(201).set_body_json(response)
        })
        .mount(mock_server)
        .await;
}

async fn mock_check_suites(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    #[derive(serde::Serialize)]
    struct CheckSuitePayload {
        conclusion: Option<String>,
        head_branch: String,
    }

    #[derive(serde::Serialize)]
    struct CheckSuiteResponse {
        check_suites: Vec<CheckSuitePayload>,
    }

    let repo_name = repo.lock().name.clone();
    dynamic_mock_req(
        move |_req: &Request, [sha]: [&str; 1]| {
            let mut repo = repo.lock();
            let Some(branch) = repo.get_branch_by_sha(sha) else {
                return ResponseTemplate::new(404);
            };
            let response = CheckSuiteResponse {
                check_suites: branch
                    .get_suites()
                    .iter()
                    .map(|suite| CheckSuitePayload {
                        conclusion: match suite {
                            CheckSuiteStatus::Pending => None,
                            CheckSuiteStatus::Success => Some("success".to_string()),
                            CheckSuiteStatus::Failure => Some("failure".to_string()),
                        },
                        head_branch: branch.name.clone(),
                    })
                    .collect(),
            };
            ResponseTemplate::new(200).set_body_json(response)
        },
        "GET",
        format!("^/repos/{repo_name}/commits/(.*)/check-suites$"),
    )
    .mount(mock_server)
    .await;
}

async fn mock_config(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    // Extracted into a block to avoid holding the lock over an await point
    let mock = {
        let repo = repo.lock();
        Mock::given(method("GET"))
            .and(path(format!(
                "/repos/{}/contents/rust-bors.toml",
                repo.name
            )))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(GitHubContent::new("rust-bors.toml", &repo.config)),
            )
            .mount(mock_server)
    };
    mock.await;
}

/// Represents all repositories for an installation
/// Returns type for the `GET /installation/repositories` endpoint
#[derive(Serialize)]
struct GitHubRepositories {
    total_count: u64,
    repositories: Vec<GitHubRepository>,
}

#[derive(Serialize)]
pub struct GitHubRepository {
    id: u64,
    name: String,
    url: Url,
    owner: GitHubUser,
}

impl From<GithubRepoName> for GitHubRepository {
    fn from(value: GithubRepoName) -> Self {
        Self {
            id: 1,
            name: value.name().to_string(),
            owner: GitHubUser::new(value.owner(), 1001),
            url: format!("https://github.com/{}", value).parse().unwrap(),
        }
    }
}

/// Represents a file in a GitHub repository
/// returns type for the `GET /repos/{owner}/{repo}/contents/{path}` endpoint
#[derive(Serialize)]
struct GitHubContent {
    name: String,
    path: String,
    sha: String,
    encoding: Option<String>,
    content: Option<String>,
    size: i64,
    url: String,
    r#type: String,
    #[serde(rename = "_links")]
    links: GitHubContentLinks,
}

impl GitHubContent {
    fn new(path: &str, content: &str) -> Self {
        let content = base64::prelude::BASE64_STANDARD.encode(content);
        let size = content.len() as i64;
        GitHubContent {
            name: path.to_string(),
            path: path.to_string(),
            sha: "test".to_string(),
            encoding: Some("base64".to_string()),
            content: Some(content),
            size,
            url: "https://test.com".to_string(),
            r#type: "file".to_string(),
            links: GitHubContentLinks {
                _self: "https://test.com".parse().unwrap(),
            },
        }
    }
}

#[derive(Serialize)]
struct GitHubContentLinks {
    #[serde(rename = "self")]
    _self: Url,
}

#[derive(Serialize)]
struct GitHubBranch {
    name: String,
    commit: GitHubCommitObject,
    protected: bool,
}

#[derive(Serialize)]
struct GitHubCommitObject {
    sha: String,
    url: Url,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct GitHubRef {
    #[serde(rename = "ref")]
    ref_field: String,
    node_id: String,
    url: Url,
    object: Object,
}
