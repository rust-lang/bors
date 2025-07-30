use std::fmt;
use std::str::FromStr;

use arc_swap::ArcSwap;
pub use command::CommandParser;
pub use command::RollupMode;
pub use comment::Comment;
pub use context::BorsContext;
pub use handlers::{handle_bors_global_event, handle_bors_repository_event};
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
pub mod mergeable_queue;

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
