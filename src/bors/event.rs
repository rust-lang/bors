use crate::github::{CommitSha, GithubRepoName, GithubUser};

#[derive(Debug)]
pub enum BorsEvent {
    Comment(PullRequestComment),
    WorkflowStarted(WorkflowStarted),
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
    pub workflow_run_id: u64,
    pub check_suite_id: u64,
}
