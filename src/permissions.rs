use axum::async_trait;
use octocrab::models::UserId;
use std::collections::HashSet;
use tokio::sync::Mutex;

use crate::github::GithubRepoName;

pub enum PermissionType {
    /// Can perform commands like r+.
    Review,
    /// Can start a try build.
    Try,
}

/// Decides if a GitHub user can perform various actions using the bot.
#[async_trait]
pub trait PermissionResolver {
    async fn has_permission(&self, _username: &UserId, _permission: PermissionType) -> bool;
    async fn reload(&self);
}

/// Loads permission information from the Rust Team API.
pub struct TeamApiPermissionResolver {
    repo: GithubRepoName,
    permissions: Mutex<UserPermissions>,
}

impl TeamApiPermissionResolver {
    pub async fn load(repo: GithubRepoName) -> anyhow::Result<Self> {
        let permissions = load_permissions(&repo).await?;

        Ok(Self {
            repo,
            permissions: Mutex::new(permissions),
        })
    }
    async fn reload_permissions(&self) {
        let result = load_permissions(&self.repo).await;
        match result {
            Ok(perms) => *self.permissions.lock().await = perms,
            Err(error) => {
                tracing::error!("Cannot reload permissions for {}: {error:?}", self.repo);
            }
        }
    }
}

#[async_trait]
impl PermissionResolver for TeamApiPermissionResolver {
    async fn has_permission(&self, user_id: &UserId, permission: PermissionType) -> bool {
        self.permissions
            .lock()
            .await
            .has_permission(user_id, permission)
    }

    async fn reload(&self) {
        self.reload_permissions().await
    }
}

pub struct UserPermissions {
    review_users: HashSet<UserId>,
    try_users: HashSet<UserId>,
}

impl UserPermissions {
    fn has_permission(&self, user_id: &UserId, permission: PermissionType) -> bool {
        match permission {
            PermissionType::Review => self.review_users.contains(user_id),
            PermissionType::Try => self.try_users.contains(user_id),
        }
    }
}

async fn load_permissions(repo: &GithubRepoName) -> anyhow::Result<UserPermissions> {
    tracing::info!("Reloading permissions for repository {repo}");

    let review_users = load_users_from_team_api(repo.name(), PermissionType::Review)
        .await
        .map_err(|error| anyhow::anyhow!("Cannot load review users: {error:?}"))?;

    let try_users = load_users_from_team_api(repo.name(), PermissionType::Try)
        .await
        .map_err(|error| anyhow::anyhow!("Cannot load try users: {error:?}"))?;
    Ok(UserPermissions {
        review_users,
        try_users,
    })
}

#[derive(serde::Deserialize)]
struct UserPermissionsResponse {
    github_ids: HashSet<UserId>,
}

/// Loads users that are allowed to perform try/review from the Rust Team API.
async fn load_users_from_team_api(
    repository_name: &str,
    permission: PermissionType,
) -> anyhow::Result<HashSet<UserId>> {
    let permission = match permission {
        PermissionType::Review => "review",
        PermissionType::Try => "try",
    };

    let normalized_name = repository_name.replace("-", "_");
    let url = format!("https://team-api.infra.rust-lang.org/v1/permissions/bors.{normalized_name}.{permission}.json");
    let users = reqwest::get(url)
        .await
        .and_then(|res| res.error_for_status())
        .map_err(|error| anyhow::anyhow!("Cannot load users from team API: {error:?}"))?
        .json::<UserPermissionsResponse>()
        .await
        .map_err(|error| anyhow::anyhow!("Cannot deserialize users from team API: {error:?}"))?;
    Ok(users.github_ids)
}
