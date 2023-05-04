use std::fmt::{Display, Formatter};

use anyhow::Context;
use axum::async_trait;
use octocrab::models::Repository;
use octocrab::Octocrab;

use crate::github::api::operations::{merge_branches, set_branch_to_commit, MergeError};
use crate::github::{CommitSha, GithubRepoName};
use crate::handlers::RepositoryClient;

pub struct PullRequestNumber(pub u64);

impl Into<PullRequestNumber> for u64 {
    fn into(self) -> PullRequestNumber {
        PullRequestNumber(self)
    }
}

impl Display for PullRequestNumber {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

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

    fn format_pr(&self, pr: PullRequestNumber) -> String {
        format!("{}/{}/{}", self.name().owner(), self.name().name(), pr)
    }
}

#[async_trait]
impl RepositoryClient for GithubRepositoryClient {
    fn repository(&self) -> &GithubRepoName {
        self.name()
    }

    /// The comment will be posted as the Github App user of the bot.
    async fn post_comment(&mut self, pr: PullRequestNumber, text: &str) -> anyhow::Result<()> {
        self.client
            .issues(&self.name().owner, &self.name().name)
            .create_comment(pr.0, text)
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
