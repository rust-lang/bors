use crate::github::GithubRepoName;
use octocrab::models::UserId;
use serde::de::DeserializeOwned;
use std::collections::HashSet;
use std::fmt;

#[derive(Eq, PartialEq, Debug, Clone)]
pub enum PermissionType {
    /// Can perform commands like r+
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

// Separate module to ensure that the struct cannot be constructed without invoking `new`
mod directory {
    use std::collections::HashSet;

    pub(super) struct TeamDirectory {
        usernames: HashSet<String>,
        teams: HashSet<String>,
    }

    impl TeamDirectory {
        pub(super) fn new(usernames: HashSet<String>, teams: HashSet<String>) -> Self {
            let usernames: HashSet<String> = usernames
                .into_iter()
                .map(|name| name.to_lowercase())
                .collect();
            let teams: HashSet<String> =
                teams.into_iter().map(|name| name.to_lowercase()).collect();
            Self { usernames, teams }
        }

        pub(super) fn user_exists(&self, username: &str) -> bool {
            self.usernames.contains(&username.to_lowercase())
        }

        pub(super) fn team_exists(&self, team: &str) -> bool {
            self.teams.contains(&team.to_lowercase())
        }
    }
}

use directory::TeamDirectory;

pub struct UserPermissions {
    review_users: HashSet<UserId>,
    try_users: HashSet<UserId>,
    directory: TeamDirectory,
}

impl UserPermissions {
    pub fn has_permission(&self, user_id: UserId, permission: PermissionType) -> bool {
        match permission {
            PermissionType::Review => self.review_users.contains(&user_id),
            PermissionType::Try => self.try_users.contains(&user_id),
        }
    }

    pub fn user_exists(&self, username: &str) -> bool {
        self.directory.user_exists(username)
    }

    pub fn team_exists(&self, team: &str) -> bool {
        self.directory.team_exists(team)
    }
}

enum TeamSource {
    Url(String),
    Directory(String),
}

pub struct TeamApiClient {
    team_source: TeamSource,
}

impl TeamApiClient {
    pub fn new(source: impl Into<String>) -> Self {
        let source_str = source.into();
        let team_source = if source_str.starts_with("http") {
            TeamSource::Url(source_str)
        } else {
            TeamSource::Directory(source_str)
        };
        Self { team_source }
    }

    pub(crate) async fn load_permissions(
        &self,
        repo: &GithubRepoName,
    ) -> anyhow::Result<UserPermissions> {
        tracing::info!("Reloading permissions for repository {repo}");

        let (review_users, try_users, known_usernames, known_teams) = tokio::try_join!(
            async {
                self.load_users(repo.name(), PermissionType::Review)
                    .await
                    .map_err(|error| anyhow::anyhow!("Cannot load review users: {error:?}"))
            },
            async {
                self.load_users(repo.name(), PermissionType::Try)
                    .await
                    .map_err(|error| anyhow::anyhow!("Cannot load try users: {error:?}"))
            },
            async {
                anyhow::Ok(
                    self.load_team_data::<rust_team_data::v1::People>("people")
                        .await
                        .map_err(|error| anyhow::anyhow!("Cannot load people: {error:?}"))?
                        .people
                        .into_keys()
                        .collect::<HashSet<String>>(),
                )
            },
            async {
                anyhow::Ok(
                    self.load_team_data::<rust_team_data::v1::Teams>("teams")
                        .await
                        .map_err(|error| anyhow::anyhow!("Cannot load teams: {error:?}"))?
                        .teams
                        .into_keys()
                        .collect::<HashSet<String>>(),
                )
            }
        )?;
        let directory = TeamDirectory::new(known_usernames, known_teams);

        Ok(UserPermissions {
            review_users,
            try_users,
            directory,
        })
    }

    /// Loads users that are allowed to perform try/review from the Rust Team API
    /// or from directory for local environment.
    async fn load_users(
        &self,
        repository_name: &str,
        permission: PermissionType,
    ) -> anyhow::Result<HashSet<UserId>> {
        let permission = match permission {
            PermissionType::Review => "review",
            PermissionType::Try => "try",
        };

        let data = match &self.team_source {
            TeamSource::Url(base_url) => {
                let normalized_name = repository_name.replace('-', "_");
                let url =
                    format!("{base_url}/v1/permissions/bors.{normalized_name}.{permission}.json",);
                reqwest::get(url)
                    .await
                    .and_then(|res| res.error_for_status())
                    .map_err(|error| anyhow::anyhow!("Cannot load users from team API: {error:?}"))?
                    .text()
                    .await?
            }
            TeamSource::Directory(base_path) => {
                let path = format!("{base_path}/bors.{permission}.json");
                std::fs::read_to_string(&path).map_err(|error| {
                    anyhow::anyhow!("Could not read users from a file '{path}': {error:?}")
                })?
            }
        };
        let permissions: rust_team_data::v1::Permission = serde_json::from_str(&data)
            .map_err(|error| anyhow::anyhow!("Cannot deserialize team permissions: {error:?}"))?;
        Ok(permissions.github_ids.into_iter().map(UserId).collect())
    }

    async fn load_team_data<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let data = match &self.team_source {
            TeamSource::Url(base_url) => {
                let url = format!("{base_url}/v1/{path}.json");
                reqwest::get(url)
                    .await
                    .and_then(|res| res.error_for_status())
                    .map_err(|error| {
                        anyhow::anyhow!("Cannot load people from team API: {error:?}")
                    })?
                    .text()
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "Cannot load {} from team: {error:?}",
                            std::any::type_name::<T>()
                        )
                    })?
            }
            TeamSource::Directory(base_path) => {
                let path = format!("{base_path}/{path}.json");
                std::fs::read_to_string(&path).map_err(|error| {
                    anyhow::anyhow!("Could not read data from file `{path}`: {error:?}")
                })?
            }
        };
        let data: T = serde_json::from_str(&data).map_err(|error| {
            anyhow::anyhow!(
                "Cannot deserialize {}: {error:?}",
                std::any::type_name::<T>()
            )
        })?;
        Ok(data)
    }
}
