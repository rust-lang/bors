mod command;
pub mod comment;
mod context;
pub mod event;
mod handlers;

use std::collections::HashMap;

use arc_swap::ArcSwap;
use axum::async_trait;

use crate::bors::event::PullRequestComment;
use crate::config::RepositoryConfig;
use crate::database::RunId;
use crate::github::{CommitSha, GithubRepoName, MergeError, PullRequest, PullRequestNumber};
use crate::permissions::UserPermissions;
use crate::TeamApiClient;
pub use command::CommandParser;
pub use comment::Comment;
pub use context::BorsContext;
pub use handlers::{handle_bors_global_event, handle_bors_repository_event};

#[cfg(test)]
pub use handlers::WAIT_FOR_REFRESH;

/// Provides functionality for working with a remote repository.
pub trait RepositoryClient: Send + Sync {
    fn repository(&self) -> &GithubRepoName;

    /// Was the comment created by the bot?
    async fn is_comment_internal(&self, comment: &PullRequestComment) -> anyhow::Result<bool>;

    /// Load repository config.
    async fn load_config(&self) -> anyhow::Result<RepositoryConfig>;

    /// Return the current SHA of the given branch.
    async fn get_branch_sha(&self, name: &str) -> anyhow::Result<CommitSha>;

    /// Resolve a pull request from this repository by it's number.
    async fn get_pull_request(&self, pr: PullRequestNumber) -> anyhow::Result<PullRequest>;

    /// Post a comment to the pull request with the given number.
    async fn post_comment(&self, pr: PullRequestNumber, comment: Comment) -> anyhow::Result<()>;

    /// Set the given branch to a commit with the given `sha`.
    async fn set_branch_to_sha(&self, branch: &str, sha: &CommitSha) -> anyhow::Result<()>;

    /// Merge `head` into `base`. Returns the SHA of the merge commit.
    async fn merge_branches(
        &self,
        base: &str,
        head: &CommitSha,
        commit_message: &str,
    ) -> Result<CommitSha, MergeError>;

    /// Find all check suites attached to the given commit and branch.
    async fn get_check_suites_for_commit(
        &self,
        branch: &str,
        sha: &CommitSha,
    ) -> anyhow::Result<Vec<CheckSuite>>;

    /// Cancels Github Actions workflows.
    async fn cancel_workflows(&self, run_ids: &[RunId]) -> anyhow::Result<()>;

    /// Add a set of labels to a PR.
    async fn add_labels(&self, pr: PullRequestNumber, labels: &[String]) -> anyhow::Result<()>;

    /// Remove a set of labels from a PR.
    async fn remove_labels(&self, pr: PullRequestNumber, labels: &[String]) -> anyhow::Result<()>;

    /// Get a workflow url.
    fn get_workflow_url(&self, run_id: RunId) -> String;
}

/// Temporary trait to sastify the test mocking.
/// TODO: Remove this trait once we move to mock REST API call.
#[async_trait]
pub trait RepositoryLoader<Client: RepositoryClient>: Send + Sync {
    /// Load state of repositories.
    async fn load_repositories(
        &self,
        team_api_client: &TeamApiClient,
    ) -> anyhow::Result<HashMap<GithubRepoName, anyhow::Result<RepositoryState<Client>>>>;
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
pub struct RepositoryState<Client: RepositoryClient> {
    pub repository: GithubRepoName,
    pub client: Client,
    pub permissions: ArcSwap<UserPermissions>,
    pub config: ArcSwap<RepositoryConfig>,
}
