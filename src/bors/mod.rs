mod command;
mod context;
pub mod event;
mod handlers;

use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::async_trait;
use octocrab::models::RunId;

use crate::bors::event::PullRequestComment;
use crate::config::RepositoryConfig;
use crate::github::{CommitSha, GithubRepoName, MergeError, PullRequest, PullRequestNumber};
use crate::permissions::UserPermissions;
pub use command::CommandParser;
pub use context::BorsContext;
pub use handlers::handle_bors_event;

/// Provides functionality for working with a remote repository.
#[async_trait]
pub trait RepositoryClient: Send + Sync {
    fn repository(&self) -> &GithubRepoName;

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

#[derive(Clone)]
pub enum CheckSuiteStatus {
    Pending,
    Failure,
    Success,
}

/// A GitHub check suite.
/// Corresponds to a single GitHub actions workflow run, or to a single external CI check run.
#[derive(Clone)]
pub struct CheckSuite {
    pub(crate) status: CheckSuiteStatus,
}

/// Main state holder for the bot.
/// It is behind a trait to allow easier mocking in tests.
#[async_trait]
pub trait BorsState<Client: RepositoryClient>: Send + Sync {
    /// Was the comment created by the bot?
    fn is_comment_internal(&self, comment: &PullRequestComment) -> bool;

    /// Get repository and database state for the given repository name.
    fn get_repo_state(&self, repo: &GithubRepoName) -> Option<Arc<RepositoryState<Client>>>;

    /// Get all repositories.
    fn get_all_repos(&self) -> Vec<Arc<RepositoryState<Client>>>;

    /// Reload state of repositories due to some external change.
    async fn reload_repositories(&self) -> anyhow::Result<()>;
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

/// A comment that can be posted to a pull request.
pub struct Comment {
    text: String,
}
impl Comment {
    pub fn new(text: String) -> Self {
        Self { text }
    }

    pub fn render(&self) -> &str {
        &self.text
    }
}
