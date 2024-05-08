use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use octocrab::models::UserId;
use serde::{Deserialize, Serialize};

use crate::github::GithubRepoName;

pub enum PermissionType {
    /// Can perform commands like r+.
    Review,
    /// Can start a try build.
    Try,
}

pub struct UserPermissions {
    review_users: HashSet<UserId>,
    try_users: HashSet<UserId>,
}

impl UserPermissions {
    pub fn new(review_users: HashSet<UserId>, try_users: HashSet<UserId>) -> Self {
        Self {
            review_users,
            try_users,
        }
    }
    pub fn has_permission(&self, user_id: &UserId, permission: PermissionType) -> bool {
        match permission {
            PermissionType::Review => self.review_users.contains(user_id),
            PermissionType::Try => self.try_users.contains(user_id),
        }
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct UserPermissionsResponse {
    pub(crate) github_ids: HashSet<UserId>,
}

pub struct TeamApiClient {
    base_url: String,
    permissions: RwLock<HashMap<GithubRepoName, UserPermissions>>,
}

impl TeamApiClient {
    pub fn new(base_url: Option<&str>) -> Self {
        let permissions = RwLock::new(HashMap::new());
        if let Some(base_url) = base_url {
            Self {
                base_url: base_url.to_string(),
                permissions,
            }
        } else {
            Self {
                base_url: "https://team-api.infra.rust-lang.org".to_string(),
                permissions,
            }
        }
    }

    pub fn has_permission(
        &self,
        repo: &GithubRepoName,
        user_id: &UserId,
        permission: PermissionType,
    ) -> bool {
        let permissions = self.permissions.read().unwrap();
        permissions
            .get(repo)
            .is_some_and(|permissions| permissions.has_permission(user_id, permission))
    }

    pub async fn fetch_permissions(&self, repo: &GithubRepoName) -> anyhow::Result<()> {
        tracing::info!("Reloading permissions for repository {repo}");

        let review_users = self
            .load_users(repo.name(), PermissionType::Review)
            .await
            .map_err(|error| anyhow::anyhow!("Cannot load review users: {error:?}"))?;

        let try_users = self
            .load_users(repo.name(), PermissionType::Try)
            .await
            .map_err(|error| anyhow::anyhow!("Cannot load try users: {error:?}"))?;
        self.permissions
            .write()
            .unwrap()
            .insert(repo.clone(), UserPermissions::new(review_users, try_users));
        Ok(())
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
        Self::new(None)
    }
}
