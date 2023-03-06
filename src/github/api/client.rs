use axum::async_trait;
use octocrab::models::Repository;
use octocrab::Octocrab;

use crate::github::{GithubRepoName, PullRequest};
use crate::handlers::RepositoryClient;

/// Provides access to a single app installation (repository).
pub struct GithubRepositoryClient {
    /// The client caches the access token for this given repository and refreshes it once it
    /// expires.
    pub client: Octocrab,
    // We store the name separately, because repository has an optional owner, but at this point
    // we must always have some owner of the repo.
    pub repo_name: GithubRepoName,
    pub repository: Repository,
}

impl GithubRepositoryClient {
    pub fn client(&self) -> &Octocrab {
        &self.client
    }

    pub fn name(&self) -> &GithubRepoName {
        &self.repo_name
    }

    pub fn repository(&self) -> &Repository {
        &self.repository
    }
}

#[async_trait]
impl RepositoryClient for GithubRepositoryClient {
    fn repository(&self) -> &GithubRepoName {
        self.name()
    }

    /// Post a comment to the pull request with the given number.
    /// The comment will be posted as the Github App user of the bot.
    async fn post_comment(&self, pr: &PullRequest, text: &str) -> anyhow::Result<()> {
        self.client
            .issues(&self.name().owner, &self.name().name)
            .create_comment(pr.number, text)
            .await?;
        Ok(())
    }
}
