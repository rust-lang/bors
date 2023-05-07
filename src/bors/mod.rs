use crate::config::RepositoryConfig;
use axum::async_trait;
use std::future::Future;
use std::pin::Pin;

use crate::github::{CommitSha, GithubRepoName, MergeError, PullRequest, PullRequestNumber};
use crate::permissions::PermissionResolver;

mod command;
pub mod event;
mod handlers;

use crate::bors::event::PullRequestComment;
use crate::database::DbClient;
pub use handlers::handle_bors_event;

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

    /// Find all check suites attached to the given commit.
    async fn get_check_suites_for_commit(
        &mut self,
        sha: &CommitSha,
    ) -> anyhow::Result<Vec<CheckSuite>>;
}

#[derive(Clone)]
pub enum CheckSuiteStatus {
    Pending,
    Failure,
    Success,
}

#[derive(Clone)]
pub struct CheckSuite {
    pub(crate) status: CheckSuiteStatus,
}

pub trait BorsState<Client: RepositoryClient> {
    /// Was the comment created by the bot?
    fn is_comment_internal(&self, comment: &PullRequestComment) -> bool;

    /// Get repository and database state for the given repository name.
    fn get_repo_state_mut(
        &mut self,
        repo: &GithubRepoName,
    ) -> Option<(&mut RepositoryState<Client>, &mut dyn DbClient)>;

    /// Reload state of repositories due to some external change.
    fn reload_repositories(&mut self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + '_>>;
}

pub struct RepositoryState<Client: RepositoryClient> {
    pub repository: GithubRepoName,
    pub client: Client,
    pub permissions_resolver: Box<dyn PermissionResolver>,
    pub config: RepositoryConfig,
}
