//! Contains definitions of common types (pull request, user, repository name) needed
//! for working with (GitHub) repositories.
use octocrab::models::UserId;
use octocrab::models::pulls::MergeableState;
use std::fmt::{Debug, Display, Formatter};
use std::str::FromStr;
use url::Url;

pub mod api;
mod error;
mod labels;
mod rollup;
pub mod server;
mod webhook;

pub use api::operations::{MergeResult, attempt_merge};
pub use error::AppError;
pub use labels::{LabelModification, LabelTrigger};
pub use webhook::WebhookSecret;

use crate::bors::PullRequestStatus;

/// Unique identifier of a GitHub repository
#[derive(Debug, PartialEq, Eq, Hash, Clone, PartialOrd, Ord)]
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

// This implementation should be kept in sync with the `Display`
// implementation above.
impl FromStr for GithubRepoName {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split('/');
        let owner = parts
            .next()
            .ok_or_else(|| "GitHub repository name must not be empty".to_string())?;
        let name = parts
            .next()
            .ok_or("GitHub repository name must be in the format `<owner>/<name>`")?;
        Ok(Self::new(owner, name))
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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
    pub mergeable_state: MergeableState,
    pub message: String,
    pub author: GithubUser,
    pub assignees: Vec<GithubUser>,
    pub status: PullRequestStatus,
    pub labels: Vec<String>,
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
            assignees: pr
                .assignees
                .unwrap_or_default()
                .into_iter()
                .map(|a| a.into())
                .collect(),
            title: pr.title.unwrap_or_default(),
            message: pr.body.unwrap_or_default(),
            mergeable_state: pr.mergeable_state.unwrap_or(MergeableState::Unknown),
            status: if pr.draft == Some(true) {
                PullRequestStatus::Draft
            } else if pr.merged_at.is_some() {
                PullRequestStatus::Merged
            } else if pr.closed_at.is_some() {
                PullRequestStatus::Closed
            } else {
                PullRequestStatus::Open
            },
            labels: pr
                .labels
                .unwrap_or_default()
                .into_iter()
                .map(|l| l.name)
                .collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
