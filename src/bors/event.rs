use crate::github::{GithubRepoName, GithubUser};

#[derive(Debug)]
pub enum BorsEvent {
    Comment(PullRequestComment),
    InstallationsChanged,
}

#[derive(Debug)]
pub struct PullRequestComment {
    pub repository: GithubRepoName,
    pub author: GithubUser,
    pub pr_number: u64,
    pub text: String,
}
