use axum::async_trait;
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use crate::github::GithubRepoName;

pub enum PermissionType {
    Review,
    Try,
}

#[async_trait]
pub trait PermissionResolver {
    async fn has_permission(&mut self, username: &str, permission: PermissionType) -> bool;
}

/// For how long should the permissions be cached.
const CACHE_DURATION: Duration = Duration::from_secs(60 * 1);

pub struct TeamApiPermissionResolver {
    repo: GithubRepoName,
    permissions: CachedUserPermissions,
}

impl TeamApiPermissionResolver {
    pub async fn load(repo: GithubRepoName) -> anyhow::Result<Self> {
        let permissions = load_permissions(&repo).await?;

        Ok(Self {
            repo,
            permissions: CachedUserPermissions::new(permissions),
        })
    }

    async fn reload_permissions(&mut self) {
        let result = load_permissions(&self.repo).await;
        match result {
            Ok(perms) => self.permissions = CachedUserPermissions::new(perms),
            Err(error) => {
                log::error!("Cannot reload permissions for {}: {error:?}", self.repo);
            }
        }
    }
}

#[async_trait]
impl PermissionResolver for TeamApiPermissionResolver {
    async fn has_permission(&mut self, username: &str, permission: PermissionType) -> bool {
        if self.permissions.is_stale() {
            self.reload_permissions().await;
        }

        match permission {
            PermissionType::Review => self.permissions.permissions.review_users.contains(username),
            PermissionType::Try => self.permissions.permissions.try_users.contains(username),
        }
    }
}

struct UserPermissions {
    review_users: HashSet<String>,
    try_users: HashSet<String>,
}

struct CachedUserPermissions {
    permissions: UserPermissions,
    created_at: SystemTime,
}
impl CachedUserPermissions {
    fn new(permissions: UserPermissions) -> Self {
        Self {
            permissions,
            created_at: SystemTime::now(),
        }
    }

    fn is_stale(&self) -> bool {
        self.created_at
            .elapsed()
            .map(|duration| duration > CACHE_DURATION)
            .unwrap_or(true)
    }
}

async fn load_permissions(repo: &GithubRepoName) -> anyhow::Result<UserPermissions> {
    log::info!("Reloading permissions for repository {repo}");

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
    github_users: HashSet<String>,
}

/// Loads users that are allowed to perform try/review from the Rust Team API.
async fn load_users_from_team_api(
    repository_name: &str,
    permission: PermissionType,
) -> anyhow::Result<HashSet<String>> {
    let permission = match permission {
        PermissionType::Review => "review",
        PermissionType::Try => "try",
    };

    let url = format!("https://team-api.infra.rust-lang.org/v1/permissions/bors.{repository_name}.{permission}.json");
    let users = reqwest::get(url)
        .await
        .and_then(|res| res.error_for_status())
        .map_err(|error| anyhow::anyhow!("Cannot load users from team API: {error:?}"))?
        .json::<UserPermissionsResponse>()
        .await
        .map_err(|error| anyhow::anyhow!("Cannot deserialize users from team API: {error:?}"))?;
    Ok(users.github_users)
}
