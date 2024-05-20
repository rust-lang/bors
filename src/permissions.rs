use octocrab::models::UserId;
use std::collections::HashSet;

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

pub async fn load_permissions(repo: &GithubRepoName) -> anyhow::Result<UserPermissions> {
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

    let normalized_name = repository_name.replace('-', "_");
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
