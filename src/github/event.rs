use crate::github::GithubRepoName;
use url::Url;

#[derive(Debug, PartialEq)]
pub struct GithubUser {
    pub username: String,
    pub html_url: Url,
}

#[derive(Debug, PartialEq)]
pub struct PullRequestComment {
    pub repository: GithubRepoName,
    pub user: GithubUser,
    pub pr_number: u64,
    pub text: String,
}

#[derive(Debug, PartialEq)]
pub enum WebhookEvent {
    Comment(PullRequestComment),
    InstallationsChanged,
}
