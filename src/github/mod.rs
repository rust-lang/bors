use std::fmt::{Display, Formatter};

use crate::database::RepositoryId;
use secrecy::{ExposeSecret, SecretString};
use url::Url;

pub mod api;
pub mod process;
pub mod server;
pub mod webhook;

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

impl From<&GithubRepoName> for RepositoryId {
    fn from(value: &GithubRepoName) -> Self {
        RepositoryId(format!("{}/{}", value.owner, value.name))
    }
}

/// Wrapper for a secret which is zeroed on drop and can be exposed only through the [`WebhookSecret::expose`] method.
pub struct WebhookSecret(SecretString);

impl WebhookSecret {
    pub fn new(secret: String) -> Self {
        Self(secret.into())
    }

    pub fn expose(&self) -> &str {
        self.0.expose_secret().as_str()
    }
}

#[derive(Debug, PartialEq)]
pub struct GithubUser {
    pub username: String,
    pub html_url: Url,
}

#[derive(Clone, Debug)]
pub struct PullRequest {
    pub number: u64,
    pub head_label: String,
    pub head_ref: String,
    pub base_ref: String,
}
