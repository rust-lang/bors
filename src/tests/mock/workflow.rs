use crate::database::WorkflowStatus;
use crate::tests::Repo;
use crate::tests::github::{WorkflowEventKind, WorkflowRun};
use crate::tests::mock::repository::GitHubRepository;
use chrono::{DateTime, Utc};
use octocrab::models::workflows::{Conclusion, Status};
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
    pub fn new(repo: &Repo, run: WorkflowRun, event: WorkflowEventKind) -> Self {
        let repository = GitHubRepository::from(repo);
        let action = match &event {
            WorkflowEventKind::Started => "requested",
            WorkflowEventKind::Completed { .. } => "completed",
        }
        .to_string();
        Self {
            action,
            workflow_run: GitHubWorkflowRun::new(repo, run),
            repository,
        }
    }
}

#[derive(Serialize)]
pub struct GitHubWorkflowRun {
    id: RunId,
    workflow_id: WorkflowId,
    node_id: String,
    name: String,
    head_branch: String,
    head_sha: String,
    run_number: i64,
    event: String,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    conclusion: Option<Conclusion>,
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

impl GitHubWorkflowRun {
    pub fn new(repo: &Repo, run: WorkflowRun) -> Self {
        let make_url = |prefix: &str| -> Url {
            format!(
                "https://{prefix}github.com/{}/actions/runs/{}",
                repo.full_name(),
                run.run_id()
            )
            .parse()
            .unwrap()
        };

        let completed_at = Utc::now();
        let created_at = completed_at - run.duration();

        let status = match run.status() {
            WorkflowStatus::Pending => Status::Pending,
            WorkflowStatus::Success => Status::Completed,
            WorkflowStatus::Failure => Status::Failed,
        };
        let conclusion = match run.status() {
            WorkflowStatus::Pending => None,
            WorkflowStatus::Success => Some(Conclusion::Success),
            WorkflowStatus::Failure => Some(Conclusion::Failure),
        };

        let repository = GitHubRepository::from(repo);
        Self {
            id: run.run_id(),
            workflow_id: 1.into(),
            node_id: "".to_string(),
            name: run.name().to_owned(),
            head_branch: run.head_branch().to_owned(),
            head_sha: run.head_sha().to_owned(),
            run_number: 0,
            event: "".to_string(),
            status,
            conclusion,
            created_at,
            updated_at: completed_at,
            url: make_url("api."),
            html_url: make_url(""),
            jobs_url: make_url("jobs."),
            logs_url: make_url("logs."),
            check_suite_url: make_url("check-suite."),
            check_suite_id: run.check_suite_id(),
            artifacts_url: make_url("artifacts."),
            cancel_url: make_url("cancel."),
            rerun_url: make_url("rerun."),
            workflow_url: make_url("workflow."),
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
        }
    }
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
