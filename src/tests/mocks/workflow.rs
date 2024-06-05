use chrono::{DateTime, Utc};
use octocrab::models::{CheckRunId, RunId, WorkflowId};
use serde::Serialize;
use url::Url;

use crate::github::GithubRepoName;
use crate::tests::mocks::app::GitHubApp;
use crate::tests::mocks::repository::{Branch, GitHubRepository};
use crate::tests::mocks::{default_repo_name, User};

pub struct CheckSuite {
    repo: GithubRepoName,
    branch: Branch,
}

impl CheckSuite {
    pub fn completed(branch: Branch) -> Self {
        Self {
            repo: default_repo_name(),
            branch,
        }
    }
}

#[derive(Serialize)]
pub struct GitHubCheckSuiteEventPayload {
    action: String,
    check_suite: GitHubCheckSuiteInner,
    repository: GitHubRepository,
}

impl From<CheckSuite> for GitHubCheckSuiteEventPayload {
    fn from(value: CheckSuite) -> Self {
        Self {
            action: "completed".to_string(),
            check_suite: GitHubCheckSuiteInner {
                head_branch: value.branch.get_name().to_string(),
                head_sha: value.branch.get_sha().to_string(),
            },
            repository: value.repo.into(),
        }
    }
}

#[derive(Serialize)]
struct GitHubCheckSuiteInner {
    head_branch: String,
    head_sha: String,
}

#[derive(Clone)]
pub struct WorkflowEvent {
    pub event: WorkflowEventKind,
    pub workflow: Workflow,
}

impl WorkflowEvent {
    pub fn started<W: Into<Workflow>>(workflow: W) -> WorkflowEvent {
        Self {
            event: WorkflowEventKind::Started,
            workflow: workflow.into(),
        }
    }
    pub fn success<W: Into<Workflow>>(workflow: W) -> Self {
        Self {
            event: WorkflowEventKind::Completed {
                status: "success".to_string(),
            },
            workflow: workflow.into(),
        }
    }
    pub fn failure<W: Into<Workflow>>(workflow: W) -> Self {
        Self {
            event: WorkflowEventKind::Completed {
                status: "failure".to_string(),
            },
            workflow: workflow.into(),
        }
    }
}

#[derive(Clone)]
pub enum WorkflowEventKind {
    Started,
    Completed { status: String },
}

#[derive(Clone)]
pub struct Workflow {
    pub repository: GithubRepoName,
    name: String,
    run_id: u64,
    pub head_branch: String,
    head_sha: String,
    pub external: bool,
}

impl Workflow {
    pub fn with_run_id(self, run_id: u64) -> Self {
        Self { run_id, ..self }
    }
    pub fn make_external(mut self) -> Self {
        self.external = true;
        self
    }

    fn new(branch: Branch) -> Self {
        Self {
            repository: default_repo_name(),
            name: "Workflow1".to_string(),
            run_id: 1,
            head_branch: branch.get_name().to_string(),
            head_sha: branch.get_sha().to_string(),
            external: false,
        }
    }
}

impl From<Branch> for Workflow {
    fn from(value: Branch) -> Self {
        Self::new(value)
    }
}

#[derive(Copy, Clone)]
pub enum TestWorkflowStatus {
    Success,
    Failure,
}

#[derive(Serialize)]
pub struct GitHubWorkflowEventPayload {
    action: String,
    workflow_run: GitHubWorkflowRun,
    repository: GitHubRepository,
}

impl From<WorkflowEvent> for GitHubWorkflowEventPayload {
    fn from(value: WorkflowEvent) -> Self {
        let WorkflowEvent { event, workflow } = value;
        assert!(!workflow.external);

        let url: Url = format!(
            "https://github.com/workflows/{}/{}",
            workflow.name, workflow.run_id
        )
        .parse()
        .unwrap();
        Self {
            action: match &event {
                WorkflowEventKind::Started => "requested",
                WorkflowEventKind::Completed { .. } => "completed",
            }
            .to_string(),
            workflow_run: GitHubWorkflowRun {
                id: workflow.run_id.into(),
                workflow_id: 1.into(),
                node_id: "".to_string(),
                name: workflow.name,
                head_branch: workflow.head_branch,
                head_sha: workflow.head_sha,
                run_number: 0,
                event: "".to_string(),
                status: "".to_string(),
                conclusion: match event {
                    WorkflowEventKind::Started => None,
                    WorkflowEventKind::Completed { status } => Some(status),
                },
                created_at: Default::default(),
                updated_at: Default::default(),
                url: url.clone(),
                html_url: url.clone(),
                jobs_url: url.clone(),
                logs_url: url.clone(),
                check_suite_url: url.clone(),
                artifacts_url: url.clone(),
                cancel_url: url.clone(),
                rerun_url: url.clone(),
                workflow_url: url.clone(),
                head_commit: GitHubHeadCommit {
                    id: "".to_string(),
                    tree_id: "".to_string(),
                    message: "".to_string(),
                    timestamp: Default::default(),
                    author: GitHubCommitAuthor {
                        name: "".to_string(),
                        email: "".to_string(),
                    },
                    committer: GitHubCommitAuthor {
                        name: "".to_string(),
                        email: "".to_string(),
                    },
                },
                repository: workflow.repository.clone().into(),
            },
            repository: workflow.repository.into(),
        }
    }
}

#[derive(Serialize)]
pub struct GitHubCheckRunEventPayload {
    action: String,
    check_run: GitHubCheckRunInner,
    repository: GitHubRepository,
}

impl From<Workflow> for GitHubCheckRunEventPayload {
    fn from(workflow: Workflow) -> Self {
        assert!(workflow.external);

        let mut app = GitHubApp::default();
        // We need the owner not to be GitHub
        app.owner = User::new(1234, "external-ci").into();
        Self {
            action: "created".to_string(),
            check_run: GitHubCheckRunInner {
                check_run: GitHubCheckRun {
                    id: workflow.run_id.into(),
                    html_url: format!("https://external-ci.com/workflows/{}", workflow.run_id),
                },
                name: workflow.name,
                check_suite: GitHubCheckSuiteInner {
                    head_branch: workflow.head_branch,
                    head_sha: workflow.head_sha,
                },
                app,
            },
            repository: workflow.repository.into(),
        }
    }
}

#[derive(Serialize)]
struct GitHubCheckRunInner {
    #[serde(flatten)]
    check_run: GitHubCheckRun,
    name: String,
    check_suite: GitHubCheckSuiteInner,
    app: GitHubApp,
}

#[derive(Serialize)]
struct GitHubCheckRun {
    id: CheckRunId,
    html_url: String,
}

#[derive(Serialize)]
struct GitHubWorkflowRun {
    id: RunId,
    workflow_id: WorkflowId,
    node_id: String,
    name: String,
    head_branch: String,
    head_sha: String,
    run_number: i64,
    event: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    conclusion: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    url: Url,
    html_url: Url,
    jobs_url: Url,
    logs_url: Url,
    check_suite_url: Url,
    artifacts_url: Url,
    cancel_url: Url,
    rerun_url: Url,
    workflow_url: Url,
    // This is not correct! E.g. I see stuff that is only a subset of PullRequest, e.g.
    // "pull_requests":[{"url":"https://api.github.com/repos/artichoke/artichoke/pulls/1346","id":717179206,"number":1346,"head":{"ref":"pernosco-integration","sha":"9ee4335ecfc3e7abe44bddadb117a23d0d63e4ee","repo":{"id":199196552,"url":"https://api.github.com/repos/artichoke/artichoke","name":"artichoke"}},"base":{"ref":"trunk","sha":"abbb7cf0c75ab51b84309ac547c3c3c089dd36eb","repo":{"id":199196552,"url":"https://api.github.com/repos/artichoke/artichoke","name":"artichoke"}}}]
    // pull_requests: Vec<super::pulls::PullRequest>,
    // TODO: other attrs
    // ref: https://docs.github.com/en/rest/reference/actions#list-workflow-runs
    head_commit: GitHubHeadCommit,
    repository: GitHubRepository,
}

#[derive(Serialize)]
struct GitHubHeadCommit {
    id: String,
    tree_id: String,
    message: String,
    timestamp: DateTime<Utc>,
    author: GitHubCommitAuthor,
    committer: GitHubCommitAuthor,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct GitHubCommitAuthor {
    name: String,
    email: String,
}
