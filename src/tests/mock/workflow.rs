use crate::tests::github::WorkflowEventKind;
use crate::tests::mock::repository::GitHubRepository;
use crate::tests::{Repo, WorkflowEvent};
use chrono::{DateTime, Utc};
use octocrab::models::{CheckSuiteId, RunId, WorkflowId};
use serde::Serialize;
use url::Url;

#[derive(Serialize)]
pub struct GitHubWorkflowEventPayload {
    action: String,
    workflow_run: GitHubWorkflowRun,
    repository: GitHubRepository,
}

impl GitHubWorkflowEventPayload {
    pub fn new(repo: &Repo, event: WorkflowEvent) -> Self {
        let WorkflowEvent { event, workflow } = event;

        let url: Url = format!(
            "https://github.com/{}/actions/runs/{}",
            workflow.repository, workflow.run_id
        )
        .parse()
        .unwrap();

        let completed_at = Utc::now();
        let created_at = completed_at - workflow.duration;

        let repository = GitHubRepository::from(repo);
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
                created_at,
                updated_at: completed_at,
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
                repository: repository.clone(),
            },
            repository,
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
