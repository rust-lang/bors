use axum::async_trait;

use crate::github::api::client::PullRequestNumber;
use crate::github::api::operations::MergeError;
use crate::github::{CommitSha, GithubRepoName, PullRequest};

pub mod ping;
pub mod trybuild;

/// Provides functionality for working with a remote repository.
#[async_trait]
pub trait RepositoryClient {
    fn repository(&self) -> &GithubRepoName;

    /// Resolve a pull request from this repository by it's number.
    async fn get_pull_request(&mut self, pr: PullRequestNumber) -> anyhow::Result<PullRequest>;

    /// Post a comment to the pull request with the given number.
    async fn post_comment(&mut self, pr: PullRequestNumber, text: &str) -> anyhow::Result<()>;

    /// Set the given branch to a commit with the given `sha`.
    async fn set_branch_to_sha(&mut self, branch: &str, sha: &CommitSha) -> anyhow::Result<()>;

    /// Merge `head` into `base`. Returns the SHA of the merge commit.
    async fn merge_branches(
        &mut self,
        base: &str,
        head: &CommitSha,
        commit_message: &str,
    ) -> Result<CommitSha, MergeError>;
}
