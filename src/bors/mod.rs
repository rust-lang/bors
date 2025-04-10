use std::fmt;
use std::str::FromStr;

use arc_swap::ArcSwap;

pub use command::CommandParser;
pub use command::RollupMode;
pub use comment::Comment;
pub use context::BorsContext;
#[cfg(test)]
pub use handlers::WAIT_FOR_REFRESH;
pub use handlers::{handle_bors_global_event, handle_bors_repository_event};
use serde::Serialize;

use crate::config::RepositoryConfig;
use crate::github::GithubRepoName;
use crate::github::api::client::GithubRepositoryClient;
use crate::permissions::UserPermissions;

mod command;
pub mod comment;
mod context;
pub mod event;
mod handlers;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckSuiteStatus {
    Pending,
    Failure,
    Success,
}

/// A GitHub check suite.
/// Corresponds to a single GitHub actions workflow run, or to a single external CI check run.
#[derive(Clone, Debug)]
pub struct CheckSuite {
    pub(crate) status: CheckSuiteStatus,
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

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
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
