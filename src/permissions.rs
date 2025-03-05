use octocrab::models::UserId;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

use crate::github::GithubRepoName;

#[derive(Eq, PartialEq, Debug, Clone)]
pub enum PermissionType {
    /// Can perform commands like r+ and tree closed operations.
    Review,
    /// Can start a try build.
    Try,
}

impl fmt::Display for PermissionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PermissionType::Review => write!(f, "review"),
            PermissionType::Try => write!(f, "try"),
        }
    }
}

pub struct UserPermissions {
    review_users: HashSet<UserId>,
    try_users: HashSet<UserId>,
}

impl UserPermissions {
    pub fn new(
        review_users: HashSet<UserId>, 
        try_users: HashSet<UserId>,
    ) -> Self {
        Self {
            review_users,
            try_users,
        }
    }
    
    pub fn has_permission(&self, user_id: UserId, permission: PermissionType) -> bool {
        match permission {
            PermissionType::Review => self.review_users.contains(&user_id),
            PermissionType::Try => self.try_users.contains(&user_id),
        }
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct UserPermissionsResponse {
    github_ids: HashSet<UserId>,
}

pub struct TeamApiClient {
    base_url: String,
}

impl TeamApiClient {
    pub(crate) fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }

    pub(crate) async fn load_permissions(
        &self,
        repo: &GithubRepoName,
    ) -> anyhow::Result<UserPermissions> {
        tracing::info!("Reloading permissions for repository {repo}");

        let review_users = self
            .load_users(repo.name(), PermissionType::Review)
            .await
            .map_err(|error| anyhow::anyhow!("Cannot load review users: {error:?}"))?;

        let try_users = self
            .load_users(repo.name(), PermissionType::Try)
            .await
            .map_err(|error| anyhow::anyhow!("Cannot load try users: {error:?}"))?;

        Ok(UserPermissions {
            review_users,
            try_users,
        })
    }

    /// Loads users that are allowed to perform try/review from the Rust Team API.
    async fn load_users(
        &self,
        repository_name: &str,
        permission: PermissionType,
    ) -> anyhow::Result<HashSet<UserId>> {
        let permission = match permission {
            PermissionType::Review => "review",
            PermissionType::Try => "try",
        };

        let normalized_name = repository_name.replace('-', "_");
        let url = format!(
            "{}/v1/permissions/bors.{normalized_name}.{permission}.json",
            self.base_url
        );
        let users = reqwest::get(url)
            .await
            .and_then(|res| res.error_for_status())
            .map_err(|error| anyhow::anyhow!("Cannot load users from team API: {error:?}"))?
            .json::<UserPermissionsResponse>()
            .await
            .map_err(|error| {
                anyhow::anyhow!("Cannot deserialize users from team API: {error:?}")
            })?;
        Ok(users.github_ids)
    }
}

impl Default for TeamApiClient {
    fn default() -> Self {
        Self::new("https://team-api.infra.rust-lang.org")
    }
}
