use crate::database::WorkflowStatus;
use crate::github::GithubRepoName;
use crate::tests::mocks::default_repo_name;
use crate::tests::mocks::repository::{Branch, GitHubRepository};
use chrono::{DateTime, Utc};
use octocrab::models::{CheckSuiteId, JobId, RunId, WorkflowId};
use serde::Serialize;
use url::Url;

#[derive(Clone)]
pub struct WorkflowEvent {
    pub event: WorkflowEventKind,
    pub workflow: WorkflowRunData,
}

impl WorkflowEvent {
    pub fn started<W: Into<WorkflowRunData>>(workflow: W) -> WorkflowEvent {
        Self {
            event: WorkflowEventKind::Started,
            workflow: workflow.into(),
        }
    }
    pub fn success<W: Into<WorkflowRunData>>(workflow: W) -> Self {
        Self {
            event: WorkflowEventKind::Completed {
                status: "success".to_string(),
            },
            workflow: workflow.into(),
        }
    }
    pub fn failure<W: Into<WorkflowRunData>>(workflow: W) -> Self {
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
pub struct WorkflowJob {
    pub id: JobId,
    pub status: WorkflowStatus,
}

#[derive(Clone)]
pub struct WorkflowRunData {
    pub repository: GithubRepoName,
    name: String,
    pub run_id: RunId,
    pub check_suite_id: CheckSuiteId,
    pub head_branch: String,
    pub jobs: Vec<WorkflowJob>,
    head_sha: String,
}

impl WorkflowRunData {
    fn new(branch: Branch) -> Self {
        Self {
            repository: default_repo_name(),
            name: "Workflow1".to_string(),
            run_id: RunId(1),
            check_suite_id: CheckSuiteId(1),
            head_branch: branch.get_name().to_string(),
            jobs: vec![],
            head_sha: branch.get_sha().to_string(),
        }
    }

    pub fn with_run_id(self, run_id: u64) -> Self {
        Self {
            run_id: RunId(run_id),
            ..self
        }
    }

    pub fn with_check_suite_id(self, check_suite_id: u64) -> Self {
        Self {
            check_suite_id: CheckSuiteId(check_suite_id),
            ..self
        }
    }
}

impl From<Branch> for WorkflowRunData {
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

        let url: Url = format!(
            "https://github.com/{}/actions/runs/{}",
            workflow.repository, workflow.run_id
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
                id: workflow.run_id,
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
                check_suite_id: workflow.check_suite_id,
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
    check_suite_id: CheckSuiteId,
    artifacts_url: Url,
    cancel_url: Url,
    rerun_url: Url,
    workflow_url: Url,
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
