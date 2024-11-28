use arc_swap::ArcSwap;

pub use command::CommandParser;
pub use comment::Comment;
pub use context::BorsContext;
#[cfg(test)]
pub use handlers::WAIT_FOR_REFRESH;
pub use handlers::{handle_bors_global_event, handle_bors_repository_event};

use crate::config::RepositoryConfig;
use crate::github::api::client::GithubRepositoryClient;
use crate::github::GithubRepoName;
use crate::permissions::UserPermissions;

mod command;
pub mod comment;
mod context;
pub mod event;
mod handlers;

#[derive(Clone, Debug)]
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
