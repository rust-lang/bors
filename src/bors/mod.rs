use std::collections::HashMap;

use arc_swap::ArcSwap;
use octocrab::Octocrab;

pub use command::CommandParser;
pub use comment::Comment;
pub use context::BorsContext;
#[cfg(test)]
pub use handlers::WAIT_FOR_REFRESH;
pub use handlers::{handle_bors_global_event, handle_bors_repository_event};

use crate::config::RepositoryConfig;
use crate::github::api::client::GithubRepositoryClient;
use crate::github::api::load_repositories;
use crate::github::GithubRepoName;
use crate::permissions::UserPermissions;
use crate::TeamApiClient;

mod command;
pub mod comment;
mod context;
pub mod event;
mod handlers;

/// Loads repositories through a global GitHub client.
pub struct RepositoryLoader {
    client: Octocrab,
}

impl RepositoryLoader {
    pub fn new(client: Octocrab) -> Self {
        Self { client }
    }

    /// Load state of repositories.
    pub async fn load_repositories(
        &self,
        team_api_client: &TeamApiClient,
    ) -> anyhow::Result<HashMap<GithubRepoName, anyhow::Result<RepositoryState>>> {
        load_repositories(&self.client, team_api_client).await
    }
}

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
    pub repository: GithubRepoName,
    pub client: GithubRepositoryClient,
    pub permissions: ArcSwap<UserPermissions>,
    pub config: ArcSwap<RepositoryConfig>,
}
