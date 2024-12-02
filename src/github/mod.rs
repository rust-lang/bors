//! Contains definitions of common types (pull request, user, repository name) needed
//! for working with (GitHub) repositories.
use std::fmt::{Debug, Display, Formatter};

use octocrab::models::UserId;
use serde::Deserialize;
use url::Url;

pub mod api;
mod labels;
pub mod server;
mod webhook;

pub use api::operations::MergeError;
pub use labels::{LabelModification, LabelTrigger};
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

impl From<String> for GithubRepoName {
    fn from(value: String) -> Self {
        let mut parts = value.split('/');
        let owner = parts.next().unwrap_or_default();
        let name = parts.next().unwrap_or_default();
        Self::new(owner, name)
    }
}

impl<'de> Deserialize<'de> for GithubRepoName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let full_name = String::deserialize(deserializer)?;
        Ok(GithubRepoName::from(full_name))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GithubUser {
    pub id: UserId,
    pub username: String,
    pub html_url: Url,
}

impl From<octocrab::models::Author> for GithubUser {
    fn from(value: octocrab::models::Author) -> Self {
        Self {
            id: value.id,
            username: value.login,
            html_url: value.html_url,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
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
    pub number: PullRequestNumber,
    // <author>:<branch>
    pub head_label: String,
    pub head: Branch,
    pub base: Branch,
    pub title: String,
    pub message: String,
    pub author: GithubUser,
}

impl From<octocrab::models::pulls::PullRequest> for PullRequest {
    fn from(pr: octocrab::models::pulls::PullRequest) -> Self {
        Self {
            number: pr.number.into(),
            head_label: pr.head.label.unwrap_or_else(|| "<unknown>".to_string()),
            head: Branch {
                name: pr.head.ref_field,
                sha: pr.head.sha.into(),
            },
            base: Branch {
                name: pr.base.ref_field,
                sha: pr.base.sha.into(),
            },
            // For some reason, author field is optional in Octocrab, but
            // they actually are not optional in Github API schema.
            author: (*pr.user.unwrap()).into(),
            title: pr.title.unwrap_or_default(),
            message: pr.body.unwrap_or_default(),
        }
    }
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
