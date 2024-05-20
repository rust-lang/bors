//! Contains definitions of common types (pull request, user, repository name) needed
//! for working with (GitHub) repositories.
use std::fmt::{Debug, Display, Formatter};

use octocrab::models::UserId;
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

impl sqlx::Type<sqlx::Postgres> for GithubRepoName {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <String as sqlx::Type<sqlx::Postgres>>::type_info()
    }
}

impl sqlx::Encode<'_, sqlx::Postgres> for GithubRepoName {
    fn encode_by_ref(&self, buf: &mut sqlx::postgres::PgArgumentBuffer) -> sqlx::encode::IsNull {
        <String as sqlx::Encode<sqlx::Postgres>>::encode(self.to_string(), buf)
    }
}

impl sqlx::Decode<'_, sqlx::Postgres> for GithubRepoName {
    fn decode(value: sqlx::postgres::PgValueRef<'_>) -> Result<Self, sqlx::error::BoxDynError> {
        let value = <String as sqlx::Decode<sqlx::Postgres>>::decode(value)?;
        Ok(Self::from(value))
    }
}

#[derive(Debug, PartialEq)]
pub struct GithubUser {
    pub id: UserId,
    pub username: String,
    pub html_url: Url,
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
}

#[derive(Clone, Copy, Debug)]
pub struct PullRequestNumber(pub u64);

impl From<u64> for PullRequestNumber {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

// to load from/ save to Postgres, as it doesn't have unsigned integer types.
impl From<i64> for PullRequestNumber {
    fn from(value: i64) -> Self {
        Self(value as u64)
    }
}

impl sqlx::Type<sqlx::Postgres> for PullRequestNumber {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        // Postgres don't have unsigned integer types.
        <i64 as sqlx::Type<sqlx::Postgres>>::type_info()
    }
}

impl Display for PullRequestNumber {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        <u64 as Display>::fmt(&self.0, f)
    }
}
