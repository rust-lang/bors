use anyhow::Context;
use axum::async_trait;
use octocrab::models::Repository;
use octocrab::Octocrab;

use crate::bors::RepositoryClient;
use crate::github::api::operations::{merge_branches, set_branch_to_commit, MergeError};
use crate::github::{Branch, CommitSha, GithubRepoName, PullRequest, PullRequestNumber};

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

    fn format_pr(&self, pr: PullRequestNumber) -> String {
        format!("{}/{}/{}", self.name().owner(), self.name().name(), pr)
    }
}

#[async_trait]
impl RepositoryClient for GithubRepositoryClient {
    fn repository(&self) -> &GithubRepoName {
        self.name()
    }

    async fn get_pull_request(&mut self, pr: PullRequestNumber) -> anyhow::Result<PullRequest> {
        let pr = self
            .client
            .pulls(self.repository().owner(), self.repository().name())
            .get(pr.0)
            .await
            .map_err(|error| {
                anyhow::anyhow!("Could not get PR {}/{}: {error:?}", self.repository(), pr.0)
            })?;
        Ok(github_pr_to_pr(pr))
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

fn github_pr_to_pr(pr: octocrab::models::pulls::PullRequest) -> PullRequest {
    PullRequest {
        number: pr.number,
        head_label: pr.head.label.unwrap_or_else(|| "<unknown>".to_string()),
        head: Branch {
            name: pr.head.ref_field,
            sha: pr.head.sha.into(),
        },
        base: Branch {
            name: pr.base.ref_field,
            sha: pr.base.sha.into(),
        },
        title: pr.title.unwrap_or_default(),
        message: pr.body.unwrap_or_default(),
    }
}
