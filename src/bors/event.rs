use crate::database::WorkflowStatus;
use crate::github::{CommitSha, GithubRepoName, GithubUser};

#[derive(Debug)]
pub enum BorsEvent {
    /// A comment was posted on a pull request.
    Comment(PullRequestComment),
    /// A workflow run on Github Actions or a check run from external CI system has been started.
    WorkflowStarted(WorkflowStarted),
    /// A workflow run on Github Actions or a check run from external CI system has been completed.
    WorkflowCompleted(WorkflowCompleted),
    /// A check suite has been completed, either as a workflow run on Github Actions, or as a
    /// workflow from some external CI system.
    CheckSuiteCompleted(CheckSuiteCompleted),
    /// The configuration of some repository has been changed for the bot's Github App.
    InstallationsChanged,
}

#[derive(Debug)]
pub struct PullRequestComment {
    pub repository: GithubRepoName,
    pub author: GithubUser,
    pub pr_number: u64,
    pub text: String,
}

#[derive(Debug)]
pub struct WorkflowStarted {
    pub repository: GithubRepoName,
    pub name: String,
    pub branch: String,
    pub commit_sha: CommitSha,
    pub run_id: Option<u64>,
    pub url: String,
}

#[derive(Debug)]
pub struct WorkflowCompleted {
    pub repository: GithubRepoName,
    pub branch: String,
    pub commit_sha: CommitSha,
    pub run_id: u64,
    pub status: WorkflowStatus,
}

#[derive(Debug)]
pub struct CheckSuiteCompleted {
    pub repository: GithubRepoName,
    pub branch: String,
    pub commit_sha: CommitSha,
}
