use crate::github::{GithubRepoName, GithubUser};

#[derive(Debug, PartialEq)]
pub struct PullRequestComment {
    pub repository: GithubRepoName,
    pub author: GithubUser,
    pub pr_number: u64,
    pub text: String,
}

#[derive(Debug, PartialEq)]
pub enum WebhookEvent {
    Comment(PullRequestComment),
    InstallationsChanged,
}
