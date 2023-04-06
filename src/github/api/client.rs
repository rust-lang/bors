use anyhow::Context;
use axum::async_trait;
use octocrab::models::Repository;
use octocrab::Octocrab;

use crate::github::api::operations::{merge_branches, set_branch_to_commit, MergeError};
use crate::github::{CommitSha, GithubRepoName, PullRequest};
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

    fn format_pr(&self, pr: &PullRequest) -> String {
        format!(
            "{}/{}/{}",
            self.name().owner(),
            self.name().name(),
            pr.number
        )
    }
}

#[async_trait]
impl RepositoryClient for GithubRepositoryClient {
    fn repository(&self) -> &GithubRepoName {
        self.name()
    }

    /// The comment will be posted as the Github App user of the bot.
    async fn post_comment(&mut self, pr: &PullRequest, text: &str) -> anyhow::Result<()> {
        self.client
            .issues(&self.name().owner, &self.name().name)
            .create_comment(pr.number, text)
            .await
            .with_context(|| format!("Cannot post comment to {}", self.format_pr(pr)))?;
        Ok(())
    }

    async fn set_branch_to_sha(&mut self, branch: &str, sha: &CommitSha) -> anyhow::Result<()> {
        Ok(set_branch_to_commit(self, branch.to_string(), sha).await?)
    }

    async fn merge_branches(
        &mut self,
        base: &str,
        head: &CommitSha,
        commit_message: &str,
    ) -> Result<CommitSha, MergeError> {
        Ok(merge_branches(self, base, head, commit_message).await?)
    }
}
