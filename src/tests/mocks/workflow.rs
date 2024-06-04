use chrono::{DateTime, Utc};
use octocrab::models::{RunId, WorkflowId};
use serde::Serialize;
use url::Url;

use crate::github::GithubRepoName;
use crate::tests::mocks::default_repo_name;
use crate::tests::mocks::repository::{Branch, GitHubRepository};

#[derive(Clone)]
pub struct Workflow {
    event: WorkflowEvent,
    repository: GithubRepoName,
    name: String,
    run_id: u64,
    head_branch: String,
    head_sha: String,
}

impl Workflow {
    pub fn started(branch: Branch) -> Self {
        Self::new(WorkflowEvent::Started, branch)
    }
    pub fn success(branch: Branch) -> Self {
        Self::new(
            WorkflowEvent::Completed {
                status: "success".to_string(),
            },
            branch,
        )
    }

    fn new(event: WorkflowEvent, branch: Branch) -> Self {
        Self {
            event,
            repository: default_repo_name(),
            name: "Workflow 1".to_string(),
            run_id: 1,
            head_branch: branch.get_name().to_string(),
            head_sha: branch.get_sha().to_string(),
        }
    }
}

#[derive(Clone)]
enum WorkflowEvent {
    Started,
    Completed { status: String },
}

#[derive(serde::Serialize)]
pub struct GitHubWorkflowEventPayload {
    action: String,
    workflow_run: GitHubWorkflowRun,
    repository: GitHubRepository,
}

impl From<Workflow> for GitHubWorkflowEventPayload {
    fn from(value: Workflow) -> Self {
        let url: Url = format!(
            "https://github.com/workflows/{}/{}",
            value.name, value.run_id
        )
        .parse()
        .unwrap();
        Self {
            action: match &value.event {
                WorkflowEvent::Started => "requested",
                WorkflowEvent::Completed { .. } => "completed",
            }
            .to_string(),
            workflow_run: GitHubWorkflowRun {
                id: value.run_id.into(),
                workflow_id: 1.into(),
                node_id: "".to_string(),
                name: value.name,
                head_branch: value.head_branch,
                head_sha: value.head_sha,
                run_number: 0,
                event: "".to_string(),
                status: "".to_string(),
                conclusion: match value.event {
                    WorkflowEvent::Started => None,
                    WorkflowEvent::Completed { status } => Some(status),
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
                repository: value.repository.clone().into(),
            },
            repository: value.repository.into(),
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
