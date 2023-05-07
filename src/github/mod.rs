//! Contains definitions of common types (pull request, user, repository name) needed
//! for working with (GitHub) repositories.
use std::fmt::{Debug, Display, Formatter};

use url::Url;

pub mod api;
pub mod server;
mod webhook;

pub use api::operations::MergeError;
pub use api::GithubAppState;
pub use webhook::WebhookSecret;

/// Unique identifier of a GitHub repository
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct GithubRepoName {
    owner: String,
    name: String,
}

impl GithubRepoName {
    pub fn new(owner: &str, name: &str) -> Self {
        Self {
            owner: owner.to_lowercase(),
            name: name.to_lowercase(),
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Display for GithubRepoName {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}/{}", self.owner, self.name))
    }
}

#[derive(Debug, PartialEq)]
pub struct GithubUser {
    pub username: String,
    pub html_url: Url,
}

#[derive(Clone, Debug)]
pub struct CommitSha(pub String);

impl From<String> for CommitSha {
    fn from(value: String) -> Self {
        Self(value)
    }
}
impl AsRef<str> for CommitSha {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}
impl Display for CommitSha {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

#[derive(Clone, Debug)]
pub struct Branch {
    pub name: String,
    pub sha: CommitSha,
}

#[derive(Clone, Debug)]
pub struct PullRequest {
    pub number: u64,
    // <author>:<branch>
    pub head_label: String,
    pub head: Branch,
    pub base: Branch,
    pub title: String,
    pub message: String,
}

#[derive(Clone, Copy, Debug)]
pub struct PullRequestNumber(pub u64);

impl From<u64> for PullRequestNumber {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl Display for PullRequestNumber {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        <u64 as Display>::fmt(&self.0, f)
    }
}
