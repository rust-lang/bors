use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use crate::github::GithubRepoName;

/// For how long should the permissions be cached.
const CACHE_DURATION: Duration = Duration::from_secs(60 * 5);

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

    pub async fn is_try_allowed(&mut self, username: &str) -> bool {
        self.reload_permissions().await;
        self.permissions.permissions.try_users.contains(username)
    }

    pub async fn is_review_allowed(&mut self, username: &str) -> bool {
        self.reload_permissions().await;
        self.permissions.permissions.review_users.contains(username)
    }

    async fn reload_permissions(&mut self) {
        if self.permissions.is_stale() {
            let result = load_permissions(&self.repo).await;
            match result {
                Ok(perms) => self.permissions = CachedUserPermissions::new(perms),
                Err(error) => {
                    log::error!("Cannot reload permissions for {}: {error:?}", self.repo);
                }
            }
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

    let review_url = format!(
        "https://team-api.infra.rust-lang.org/v1/permissions/bors.{}.review.json",
        repo.name()
    );
    let review_users = load_users(&review_url)
        .await
        .map_err(|error| anyhow::anyhow!("Cannot load review users: {error:?}"))?;

    let try_url = format!(
        "https://team-api.infra.rust-lang.org/v1/permissions/bors.{}.try.json",
        repo.name()
    );
    let try_users = load_users(&try_url)
        .await
        .map_err(|error| anyhow::anyhow!("Cannot load try users: {error:?}"))?;
    Ok(UserPermissions {
        review_users: review_users.into_iter().collect(),
        try_users: try_users.into_iter().collect(),
    })
}

#[derive(serde::Deserialize)]
struct UserPermissionsResponse {
    github_users: Vec<String>,
}

/// Loads users that are allowed to perform try/review from the Rust Team API.
async fn load_users(url: &str) -> anyhow::Result<Vec<String>> {
    let users = reqwest::get(url)
        .await
        .map_err(|error| anyhow::anyhow!("Cannot load users from team API: {error:?}"))?
        .error_for_status()?
        .json::<UserPermissionsResponse>()
        .await
        .map_err(|error| anyhow::anyhow!("Cannot deserialize users from team API: {error:?}"))?;
    Ok(users.github_users)
}
