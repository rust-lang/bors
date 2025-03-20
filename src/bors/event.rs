use crate::database::{WorkflowStatus, WorkflowType};
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
    /// When a pull request is opened.
    PullRequestOpened(PullRequestOpened),
    /// When a pull request is closed.
    PullRequestClosed(PullRequestClosed),
    // When a pull request is merged
    PullRequestMerged(PullRequestMerged),
    // When a pull request is reopened
    PullRequestReopened(PullRequestReopened),
    // When a pull request is converted to draft
    PullRequestConvertedToDraft(PullRequestConvertedToDraft),
    // When a pull request is ready for review
    PullRequestReadyForReview(PullRequestReadyForReview),
    /// When there is a push to a branch. This includes when a commit is pushed, when a commit tag is pushed,
    /// when a branch is deleted or when a tag is deleted.
    PushToBranch(PushToBranch),
    /// A workflow run on Github Actions or a check run from external CI system has been started.
    WorkflowStarted(WorkflowStarted),
    /// A workflow run on Github Actions or a check run from external CI system has been completed.
    WorkflowCompleted(WorkflowCompleted),
    /// A check suite has been completed, either as a workflow run on Github Actions, or as a
    /// workflow from some external CI system.
    CheckSuiteCompleted(CheckSuiteCompleted),
}

impl BorsRepositoryEvent {
    pub fn repository(&self) -> &GithubRepoName {
        match self {
            BorsRepositoryEvent::Comment(comment) => &comment.repository,
            BorsRepositoryEvent::PullRequestCommitPushed(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestEdited(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestOpened(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestClosed(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestMerged(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestReopened(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestConvertedToDraft(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestReadyForReview(payload) => &payload.repository,
            BorsRepositoryEvent::PushToBranch(payload) => &payload.repository,
            BorsRepositoryEvent::WorkflowStarted(workflow) => &workflow.repository,
            BorsRepositoryEvent::WorkflowCompleted(workflow) => &workflow.repository,
            BorsRepositoryEvent::CheckSuiteCompleted(payload) => &payload.repository,
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
    pub html_url: String,
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
pub struct PullRequestOpened {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
    pub draft: bool,
}

#[derive(Debug)]
pub struct PullRequestClosed {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct PullRequestMerged {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct PullRequestReopened {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct PullRequestConvertedToDraft {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct PullRequestReadyForReview {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct PushToBranch {
    pub repository: GithubRepoName,
    pub branch: String,
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
