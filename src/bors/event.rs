use crate::database::{WorkflowStatus, WorkflowType};
use crate::github::{CommitSha, GithubRepoName, GithubUser, PullRequestNumber};
use octocrab::models::RunId;

#[derive(Debug)]
pub enum BorsRepositoryEvent {
    /// A comment was posted on a pull request.
    Comment(PullRequestComment),
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
}

#[derive(Debug)]
pub struct CheckSuiteCompleted {
    pub repository: GithubRepoName,
    pub branch: String,
    pub commit_sha: CommitSha,
}
