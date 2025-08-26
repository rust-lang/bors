use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::Arc;

use super::user::{GitHubUser, User};
use crate::database::WorkflowStatus;
use crate::github::GithubRepoName;
use crate::permissions::PermissionType;
use crate::tests::mocks::dynamic_mock_req;
use crate::tests::mocks::pull_request::{PullRequest, mock_pull_requests};
use crate::tests::{GitHubState, Permissions, WorkflowJob, WorkflowRunData};
use base64::Engine;
use chrono::{DateTime, Utc};
use octocrab::models::repos::Object;
use octocrab::models::repos::Object::Commit;
use octocrab::models::workflows::{Conclusion, Status, Step};
use octocrab::models::{CheckSuiteId, JobId, RunId};
use parking_lot::Mutex;
use serde::Serialize;
use url::Url;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

#[derive(Clone, Debug)]
pub struct CheckRunData {
    pub name: String,
    pub head_sha: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub title: String,
    pub summary: String,
    pub text: String,
    pub external_id: String,
}

#[derive(Clone)]
pub struct WorkflowRun {
    workflow_run: WorkflowRunData,
    status: WorkflowStatus,
}

#[derive(Clone, Debug)]
pub enum BranchPushError {
    Conflict,
    ValidationFailed,
    InternalServerError,
}

#[derive(Clone, Debug)]
pub struct BranchPushBehaviour {
    pub error: Option<(BranchPushError, NonZeroU64)>,
}

impl BranchPushBehaviour {
    pub fn success() -> Self {
        Self { error: None }
    }

    pub fn always_fail(error_type: BranchPushError) -> Self {
        Self {
            error: Some((error_type, NonZeroU64::new(u64::MAX).unwrap())),
        }
    }

    pub fn fail_n_times(error_type: BranchPushError, count: u64) -> Self {
        Self {
            error: NonZeroU64::new(count).map(|remaining| (error_type, remaining)),
        }
    }
}

impl Default for BranchPushBehaviour {
    fn default() -> Self {
        Self::success()
    }
}

#[derive(Clone)]
pub struct Repo {
    pub name: GithubRepoName,
    pub permissions: Permissions,
    pub config: String,
    pub branches: Vec<Branch>,
    pub commit_messages: HashMap<String, String>,
    pub workflows_cancelled_by_bors: Vec<u64>,
    pub workflow_cancel_error: bool,
    /// All workflows that we know about from the side of the test.
    pub workflow_runs: Vec<WorkflowRun>,
    pub pull_requests: HashMap<u64, PullRequest>,
    pub check_runs: Vec<CheckRunData>,
    /// Cause pull request fetch to fail.
    pub pull_request_error: bool,
    /// Push error failure/success behaviour.
    pub push_behaviour: BranchPushBehaviour,
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
            commit_messages: Default::default(),
            workflows_cancelled_by_bors: vec![],
            workflow_cancel_error: false,
            workflow_runs: vec![],
            pull_request_error: false,
            pr_push_counter: 0,
            check_runs: vec![],
            push_behaviour: BranchPushBehaviour::default(),
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

    pub fn get_pr_mut(&mut self, pr: u64) -> &mut PullRequest {
        self.pull_requests.get_mut(&pr).unwrap()
    }

    pub fn get_branch_by_name(&mut self, name: &str) -> Option<&mut Branch> {
        self.branches.iter_mut().find(|b| b.name == name)
    }

    pub fn add_cancelled_workflow(&mut self, run_id: u64) {
        self.workflows_cancelled_by_bors.push(run_id);
    }

    pub fn add_check_run(&mut self, check_run: CheckRunData) {
        self.check_runs.push(check_run);
    }

    pub fn update_check_run(
        &mut self,
        check_run_id: u64,
        status: String,
        conclusion: Option<String>,
    ) {
        let check_run = self.check_runs.get_mut(check_run_id as usize).unwrap();
        check_run.status = status;
        check_run.conclusion = conclusion;
    }

    /// Inserts or updates the status of the given workflow run.
    pub fn update_workflow_run(&mut self, workflow_run: WorkflowRunData, status: WorkflowStatus) {
        if let Some(workflow) = self
            .workflow_runs
            .iter_mut()
            .find(|w| w.workflow_run.run_id == workflow_run.run_id)
        {
            workflow.status = status;
        } else {
            self.workflow_runs.push(WorkflowRun {
                workflow_run,
                status,
            });
        }
    }

    pub fn get_next_pr_push_counter(&mut self) -> u64 {
        self.pr_push_counter += 1;
        self.pr_push_counter
    }

    pub fn get_commit_message(&self, sha: &str) -> String {
        self.commit_messages
            .get(sha)
            .expect("Commit message not found")
            .clone()
    }

    pub fn set_commit_message(&mut self, sha: &str, message: &str) {
        self.commit_messages
            .insert(sha.to_string(), message.to_string());
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
merge_queue_enabled = true

# Set labels on PR approvals
[labels]
approved = ["+approved"]
"#
        .to_string();

        let mut users = HashMap::default();
        users.insert(
            User::default_pr_author(),
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

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Branch {
    name: String,
    sha: String,
    sha_history: Vec<String>,
    merge_counter: u64,
    pub merge_conflict: bool,
}

impl Branch {
    pub fn new(name: &str, sha: &str) -> Self {
        Self {
            name: name.to_string(),
            sha: sha.to_string(),
            sha_history: vec![],
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

pub async fn mock_repo(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    mock_pull_requests(repo.clone(), mock_server).await;
    mock_branches(repo.clone(), mock_server).await;
    mock_cancel_workflow(repo.clone(), mock_server).await;
    mock_check_runs(repo.clone(), mock_server).await;
    mock_workflow_runs(repo.clone(), mock_server).await;
    mock_workflow_jobs(repo.clone(), mock_server).await;
    mock_config(repo.clone(), mock_server).await;
}

async fn mock_branches(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    mock_get_branch(repo.clone(), mock_server).await;
    mock_create_branch(repo.clone(), mock_server).await;
    mock_update_branch(repo.clone(), mock_server).await;
    mock_merge_branch(repo.clone(), mock_server).await;
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

            if let Some((error_type, remaining)) = &mut repo.push_behaviour.error {
                let (message, status) = match error_type {
                    BranchPushError::Conflict => ("Conflict", 409),
                    BranchPushError::ValidationFailed => {
                        ("Validation failed, or the endpoint has been spammed.", 422)
                    }
                    BranchPushError::InternalServerError => ("Internal server error", 500),
                };

                let current = remaining.get();
                if current == 1 {
                    repo.push_behaviour.error = None;
                } else {
                    *remaining = NonZeroU64::new(current - 1).unwrap();
                }

                return ResponseTemplate::new(status).set_body_json(serde_json::json!({
                    "message": message,
                    "status": status.to_string(),
                    "documentation_url": "https://docs.github.com/rest/git/refs#update-a-reference",
                }));
            }

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
                    data.head.clone()
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

            let source_info = if head_sha.starts_with("pr-") && head_sha.ends_with("-sha") {
                head_sha.strip_suffix("-sha").unwrap()
            } else {
                // Use the original head reference (branch name or SHA)
                &data.head
            };

            let merge_sha = format!("merge-{}-{source_info}", base_branch.merge_counter);
            base_branch.merge_counter += 1;
            base_branch.set_to_sha(&merge_sha);
            repo.set_commit_message(&merge_sha, &data.commit_message);

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

async fn mock_workflow_runs(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    #[derive(serde::Serialize, Debug)]
    struct WorkflowRunResponse {
        id: octocrab::models::RunId,
        status: Status,
        conclusion: Option<Conclusion>,
    }

    #[derive(serde::Serialize, Debug)]
    struct WorkflowRunsResponse {
        workflow_runs: Vec<WorkflowRunResponse>,
    }

    let repo_name = repo.lock().name.clone();
    dynamic_mock_req(
        move |req: &Request, []| {
            let repo = repo.lock();
            let check_suite_id: CheckSuiteId = get_query_param(req, "check_suite_id")
                .parse::<u64>()
                .unwrap()
                .into();
            let workflow_runs: Vec<WorkflowRun> = repo
                .workflow_runs
                .iter()
                .filter(|w| w.workflow_run.check_suite_id == check_suite_id)
                .cloned()
                .collect();

            let response = WorkflowRunsResponse {
                workflow_runs: workflow_runs
                    .into_iter()
                    .map(|run| {
                        let (status, conclusion) = status_to_gh(run.status);
                        WorkflowRunResponse {
                            id: run.workflow_run.run_id,
                            status,
                            conclusion,
                        }
                    })
                    .collect(),
            };
            ResponseTemplate::new(200).set_body_json(response)
        },
        "GET",
        format!("^/repos/{repo_name}/actions/runs$"),
    )
    .mount(mock_server)
    .await;
}

/// Returns (status, conclusion).
fn status_to_gh(status: WorkflowStatus) -> (Status, Option<Conclusion>) {
    let conclusion = match status {
        WorkflowStatus::Success => Some(Conclusion::Success),
        WorkflowStatus::Failure => Some(Conclusion::Failure),
        _ => None,
    };
    let status = match status {
        WorkflowStatus::Pending => Status::Pending,
        WorkflowStatus::Success | WorkflowStatus::Failure => Status::Completed,
    };
    (status, conclusion)
}

async fn mock_workflow_jobs(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    let repo_name = repo.lock().name.clone();
    dynamic_mock_req(
        move |_req: &Request, [run_id]: [&str; 1]| {
            let repo = repo.lock();
            let run_id: RunId = run_id.parse::<u64>().expect("Non-integer run id").into();
            let workflow_run = repo
                .workflow_runs
                .iter()
                .find(|w| w.workflow_run.run_id == run_id)
                .unwrap_or_else(|| panic!("Workflow run with ID {run_id} not found"));

            let response = GitHubWorkflowJobs {
                total_count: workflow_run.workflow_run.jobs.len() as u64,
                jobs: workflow_run
                    .workflow_run
                    .jobs
                    .iter()
                    .map(|job| workflow_job_to_gh(job, run_id))
                    .collect(),
            };
            ResponseTemplate::new(200).set_body_json(response)
        },
        "GET",
        format!("^/repos/{repo_name}/actions/runs/(.*)/jobs"),
    )
    .mount(mock_server)
    .await;
}

fn get_query_param(req: &Request, key: &str) -> String {
    req.url
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| panic!("Query parameter {key} not found in {}", req.url))
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

#[derive(serde::Deserialize)]
struct CheckRunRequestOutput {
    title: String,
    summary: String,
    text: Option<String>,
}

#[derive(serde::Serialize)]
struct CheckRunResponse {
    id: u64,
    node_id: String,
    name: String,
    head_sha: String,
    url: String,
    html_url: String,
    details_url: Option<String>,
    status: String,
    conclusion: Option<String>,
    started_at: String,
    completed_at: Option<String>,
    external_id: String,
    output: CheckRunResponseOutput,
    pull_requests: Vec<serde_json::Value>,
}

#[derive(serde::Serialize)]
struct CheckRunResponseOutput {
    title: String,
    summary: String,
    text: Option<String>,
    annotations_count: u64,
    annotations_url: String,
}

async fn mock_check_runs(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    let repo_name = repo.lock().name.clone();
    Mock::given(method("POST"))
        .and(path(format!("/repos/{repo_name}/check-runs")))
        .respond_with({
            let repo = repo.clone();
            let repo_name = repo_name.clone();
            move |request: &Request| {
                #[derive(serde::Deserialize)]
                struct CheckRunRequest {
                    name: String,
                    head_sha: String,
                    status: String,
                    output: CheckRunRequestOutput,
                    external_id: String,
                }

                let data: CheckRunRequest = request.body_json().unwrap();
                let time = Utc::now().to_rfc3339();

                let check_run = CheckRunData {
                    name: data.name.clone(),
                    head_sha: data.head_sha.clone(),
                    status: data.status.clone(),
                    conclusion: None,
                    title: data.output.title.clone(),
                    summary: data.output.summary.clone(),
                    text: data.output.text.clone().unwrap_or_default(),
                    external_id: data.external_id.clone(),
                };

                let mut repo = repo.lock();
                repo.add_check_run(check_run);

                let check_run_id = (repo.check_runs.len() - 1) as u64;

                let response = CheckRunResponse {
                    id: check_run_id,
                    node_id: "1234".to_string(),
                    name: data.name,
                    head_sha: data.head_sha,
                    url: format!(
                        "https://api.github.com/repos/{repo_name}/check-runs/{check_run_id}"
                    ),
                    html_url: format!("https://github.com/{repo_name}/runs/{check_run_id}"),
                    details_url: None,
                    status: data.status,
                    conclusion: None,
                    started_at: time,
                    completed_at: None,
                    external_id: data.external_id,
                    output: CheckRunResponseOutput {
                        title: data.output.title,
                        summary: data.output.summary,
                        text: data.output.text,
                        annotations_count: 0,
                        annotations_url: format!(
                            "https://api.github.com/repos/{repo_name}/check-runs/{check_run_id}/annotations"
                        ),
                    },
                    pull_requests: vec![],
                };

                ResponseTemplate::new(201).set_body_json(response)
            }
        })
        .mount(mock_server)
        .await;

    Mock::given(method("PATCH"))
        .and(wiremock::matchers::path_regex(format!(
            r"/repos/{repo_name}/check-runs/\d+"
        )))
        .respond_with({
            let repo = repo.clone();
            let repo_name = repo_name.clone();
            move |request: &Request| {
                #[derive(serde::Deserialize)]
                struct UpdateCheckRunRequest {
                    status: String,
                    conclusion: Option<String>,
                }

                let path = request.url.path();
                let check_run_id = path
                    .split('/')
                    .next_back()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap();

                let data: UpdateCheckRunRequest = request.body_json().unwrap();
                let time = Utc::now().to_rfc3339();

                let mut repo = repo.lock();
                repo.update_check_run(check_run_id, data.status.clone(), data.conclusion.clone());

                let check_run = &repo.check_runs[check_run_id as usize];

                let response = CheckRunResponse {
                    id: check_run_id,
                    node_id: "1234".to_string(),
                    name: check_run.name.clone(),
                    head_sha: check_run.head_sha.clone(),
                    url: format!(
                        "https://api.github.com/repos/{repo_name}/check-runs/{check_run_id}"
                    ),
                    html_url: format!("https://github.com/{repo_name}/runs/{check_run_id}"),
                    details_url: None,
                    status: check_run.status.clone(),
                    conclusion: check_run.conclusion.clone(),
                    started_at: time.clone(),
                    completed_at: if check_run.status == "completed" {
                        Some(time)
                    } else {
                        None
                    },
                    external_id: check_run.external_id.clone(),
                    output: CheckRunResponseOutput {
                        title: check_run.title.clone(),
                        summary: check_run.summary.clone(),
                        text: Some(check_run.text.clone()),
                        annotations_count: 0,
                        annotations_url: format!(
                            "https://api.github.com/repos/{repo_name}/check-runs/{check_run_id}/annotations"
                        ),
                    },
                    pull_requests: vec![],
                };

                ResponseTemplate::new(200).set_body_json(response)
            }
        })
        .mount(mock_server)
        .await;
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
            url: format!("https://github.com/{value}").parse().unwrap(),
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

#[derive(Serialize)]
struct GitHubWorkflowJobs {
    total_count: u64,
    jobs: Vec<GitHubWorkflowJob>,
}

#[derive(Serialize)]
struct GitHubWorkflowJob {
    id: JobId,
    run_id: RunId,
    workflow_name: String,
    head_branch: String,
    run_url: Url,
    run_attempt: u32,

    node_id: String,
    head_sha: String,
    url: Url,
    html_url: Url,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    conclusion: Option<Conclusion>,
    created_at: DateTime<Utc>,
    started_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<DateTime<Utc>>,
    name: String,
    steps: Vec<Step>,
    check_run_url: String,
    labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runner_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runner_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runner_group_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runner_group_name: Option<String>,
}

fn workflow_job_to_gh(job: &WorkflowJob, run_id: RunId) -> GitHubWorkflowJob {
    let WorkflowJob { id, status } = job;
    let (status, conclusion) = status_to_gh(*status);
    GitHubWorkflowJob {
        id: *id,
        run_id,
        workflow_name: "".to_string(),
        head_branch: "".to_string(),
        run_url: "https://test.com".parse().unwrap(),
        run_attempt: 0,
        node_id: "".to_string(),
        head_sha: "".to_string(),
        url: "https://test.com".parse().unwrap(),
        html_url: format!("https://github.com/job-logs/{id}").parse().unwrap(),
        status,
        conclusion,
        created_at: Default::default(),
        started_at: Default::default(),
        completed_at: None,
        name: format!("Job {id}"),
        steps: vec![],
        check_run_url: "".to_string(),
        labels: vec![],
        runner_id: None,
        runner_name: None,
        runner_group_id: None,
        runner_group_name: None,
    }
}
