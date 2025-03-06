use crate::database::{TreeState, WorkflowStatus, WorkflowType};
use crate::github::{CommitSha, GithubRepoName, GithubUser, PullRequest, PullRequestNumber};
use chrono::Duration;
use octocrab::models::RunId;

#[derive(Debug)]
pub enum BorsRepositoryEvent {
    /// A comment was posted on a pull request.
    Comment(PullRequestComment),
    /// When a new commit is pushed to the pull request branch.
    PullRequestCommitPushed(PullRequestPushed),
    /// When the pull request is edited by its author
    PullRequestEdited(PullRequestEdited),
    /// A workflow run on Github Actions or a check run from external CI system has been started.
    WorkflowStarted(WorkflowStarted),
    /// A workflow run on Github Actions or a check run from external CI system has been completed.
    WorkflowCompleted(WorkflowCompleted),
    /// A check suite has been completed, either as a workflow run on Github Actions, or as a
    /// workflow from some external CI system.
    CheckSuiteCompleted(CheckSuiteCompleted),
    /// When a repository's tree state changes (open/closed)
    TreeStateChanged(TreeStateChanged),
}

impl BorsRepositoryEvent {
    pub fn repository(&self) -> &GithubRepoName {
        match self {
            BorsRepositoryEvent::Comment(comment) => &comment.repository,
            BorsRepositoryEvent::PullRequestCommitPushed(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestEdited(payload) => &payload.repository,
            BorsRepositoryEvent::WorkflowStarted(workflow) => &workflow.repository,
            BorsRepositoryEvent::WorkflowCompleted(workflow) => &workflow.repository,
            BorsRepositoryEvent::CheckSuiteCompleted(payload) => &payload.repository,
            BorsRepositoryEvent::TreeStateChanged(payload) => &payload.repository,
        }
    }
}

#[derive(Debug)]
pub enum BorsGlobalEvent {
    /// The configuration of some repository has been changed for the bot's Github App.
    InstallationsChanged,
    /// Periodic event that serves for checking e.g. timeouts.
    Refresh,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum BorsEvent {
    /// An event that happen per repository basis.
    Repository(BorsRepositoryEvent),
    /// An event that happen with bors globally.
    Global(BorsGlobalEvent),
}

#[derive(Debug)]
pub struct PullRequestComment {
    pub repository: GithubRepoName,
    pub author: GithubUser,
    pub pr_number: PullRequestNumber,
    pub text: String,
}

#[derive(Debug)]
pub struct PullRequestPushed {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct PullRequestEdited {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
    pub from_base_sha: Option<CommitSha>,
}

#[derive(Debug)]
pub struct WorkflowStarted {
    pub repository: GithubRepoName,
    pub name: String,
    pub branch: String,
    pub commit_sha: CommitSha,
    pub run_id: RunId,
    pub workflow_type: WorkflowType,
    pub url: String,
}

#[derive(Debug)]
pub struct WorkflowCompleted {
    pub repository: GithubRepoName,
    pub branch: String,
    pub commit_sha: CommitSha,
    pub run_id: RunId,
    pub status: WorkflowStatus,
    pub running_time: Option<Duration>,
}

#[derive(Debug)]
pub struct CheckSuiteCompleted {
    pub repository: GithubRepoName,
    pub branch: String,
    pub commit_sha: CommitSha,
}

#[derive(Debug)]
pub struct TreeStateChanged {
    pub repository: GithubRepoName,
    pub tree_state: TreeState,
    pub source: Option<String>,
}
