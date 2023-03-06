use axum::async_trait;

use crate::github::{GithubRepoName, PullRequest};

pub mod ping;
pub mod trybuild;

/// Provides functionality for working with a remote repository.
#[async_trait]
pub trait RepositoryClient {
    fn repository(&self) -> &GithubRepoName;

    async fn post_comment(&mut self, pr: &PullRequest, text: &str) -> anyhow::Result<()>;
}
