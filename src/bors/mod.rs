use std::fmt;
use std::str::FromStr;

use arc_swap::ArcSwap;
pub use command::CommandParser;
pub use command::RollupMode;
pub use comment::Comment;
pub use context::BorsContext;
pub use handlers::{handle_bors_global_event, handle_bors_repository_event};
use itertools::Itertools;
use octocrab::models::RunId;
use octocrab::models::workflows::Job;
use serde::Serialize;

use crate::config::RepositoryConfig;
use crate::github::GithubRepoName;
use crate::github::api::client::GithubRepositoryClient;
use crate::permissions::UserPermissions;
#[cfg(test)]
use crate::tests::TestSyncMarker;

mod command;
pub mod comment;
mod context;
pub mod event;
mod handlers;
pub mod merge_queue;
pub mod mergeability_queue;

use crate::database::{WorkflowModel, WorkflowStatus};
pub use command::CommandPrefix;

#[cfg(test)]
pub static WAIT_FOR_REFRESH_PENDING_BUILDS: TestSyncMarker = TestSyncMarker::new();

#[cfg(test)]
pub static WAIT_FOR_MERGEABILITY_STATUS_REFRESH: TestSyncMarker = TestSyncMarker::new();

#[cfg(test)]
pub static WAIT_FOR_PR_STATUS_REFRESH: TestSyncMarker = TestSyncMarker::new();

#[cfg(test)]
pub static WAIT_FOR_WORKFLOW_STARTED: TestSyncMarker = TestSyncMarker::new();

#[cfg(test)]
pub static WAIT_FOR_WORKFLOW_COMPLETED: TestSyncMarker = TestSyncMarker::new();

#[cfg(test)]
pub static WAIT_FOR_MERGE_QUEUE: TestSyncMarker = TestSyncMarker::new();

/// Corresponds to a single execution of a workflow.
#[derive(Clone, Debug)]
pub struct WorkflowRun {
    pub id: RunId,
    pub status: WorkflowStatus,
}

pub struct FailedWorkflowRun {
    pub workflow_run: WorkflowModel,
    pub failed_jobs: Vec<Job>,
}

/// An access point to a single repository.
/// Can be used to query permissions for the repository, and also to perform various
/// actions using the stored client.
pub struct RepositoryState {
    pub client: GithubRepositoryClient,
    pub permissions: ArcSwap<UserPermissions>,
    pub config: ArcSwap<RepositoryConfig>,
}

impl RepositoryState {
    pub fn repository(&self) -> &GithubRepoName {
        self.client.repository()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum PullRequestStatus {
    Closed,
    Draft,
    Merged,
    Open,
}

impl fmt::Display for PullRequestStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status_str = match self {
            PullRequestStatus::Closed => "closed",
            PullRequestStatus::Draft => "draft",
            PullRequestStatus::Merged => "merged",
            PullRequestStatus::Open => "open",
        };
        write!(f, "{status_str}")
    }
}

// Has to be kept in sync with the `Display` implementation above.
impl FromStr for PullRequestStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "closed" => Ok(PullRequestStatus::Closed),
            "draft" => Ok(PullRequestStatus::Draft),
            "merged" => Ok(PullRequestStatus::Merged),
            "open" => Ok(PullRequestStatus::Open),
            status => Err(format!("Invalid PR status {status}")),
        }
    }
}

/// Prefix used to specify custom try jobs in PR descriptions.
pub const CUSTOM_TRY_JOB_PREFIX: &str = "try-job:";

#[derive(Debug, Clone)]
pub enum MergeType {
    Try { try_jobs: Vec<String> },
    Auto,
}

pub fn create_merge_commit_message(pr: handlers::PullRequestData, merge_type: MergeType) -> String {
    let pr_number = pr.number();

    let reviewer = match &merge_type {
        MergeType::Try { .. } => "<try>",
        MergeType::Auto => pr.db.approver().unwrap_or("<unknown>"),
    };

    let pr_description = match &merge_type {
        // Only keep any lines starting with `CUSTOM_TRY_JOB_PREFIX`.
        MergeType::Try { try_jobs } if try_jobs.is_empty() => {
            pr.github
                .message
                .lines()
                .map(|l| l.trim())
                .filter(|l| l.starts_with(CUSTOM_TRY_JOB_PREFIX))
                .join("\n")
        }
        // If we do not have any custom try jobs, keep the ones that might be in the PR
        // description.
        MergeType::Try { .. } => String::new(),
        MergeType::Auto => pr.github.message.clone(),
    };

    let mut message = format!(
        r#"Auto merge of #{pr_number} - {pr_label}, r={reviewer}
{pr_title}

{pr_description}"#,
        pr_label = pr.github.head_label,
        pr_title = pr.github.title,
    );

    match merge_type {
        MergeType::Try { try_jobs } => {
            for job in try_jobs {
                message.push_str(&format!("\n{CUSTOM_TRY_JOB_PREFIX} {job}"));
            }
        }
        MergeType::Auto => {}
    }
    message
}
