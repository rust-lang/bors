use octocrab::models::UserId;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::github::GithubRepoName;

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

#[derive(Deserialize, Serialize)]
pub(crate) struct UserPermissionsResponse {
    github_ids: HashSet<UserId>,
}

type PeopleResponse = HashMap<String, serde_json::Value>;

type TeamResponse = HashMap<String, serde_json::Value>;

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
            self.load_people_names(),
            self.load_team_names(),
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

        match &self.team_source {
            TeamSource::Url(base_url) => {
                let normalized_name = repository_name.replace('-', "_");
                let url =
                    format!("{base_url}/v1/permissions/bors.{normalized_name}.{permission}.json",);
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
            TeamSource::Directory(base_path) => {
                let path = format!("{base_path}/bors.{permission}.json");
                let data = std::fs::read_to_string(&path).map_err(|error| {
                    anyhow::anyhow!("Could not read users from a file '{path}': {error:?}")
                })?;
                let users: UserPermissionsResponse =
                    serde_json::from_str(&data).map_err(|error| {
                        anyhow::anyhow!("Cannot deserialize users from a file '{path}': {error:?}")
                    })?;
                Ok(users.github_ids)
            }
        }
    }

    async fn load_people_names(&self) -> anyhow::Result<HashSet<String>> {
        match &self.team_source {
            TeamSource::Url(base_url) => {
                let url = format!("{base_url}/v1/people.json");
                let response = reqwest::get(url)
                    .await
                    .and_then(|res| res.error_for_status())
                    .map_err(|error| {
                        anyhow::anyhow!("Cannot load people from team API: {error:?}")
                    })?
                    .json::<PeopleResponse>()
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("Cannot deserialize people from team API: {error:?}")
                    })?
                    .into_keys()
                    .collect();
                Ok(response)
            }
            TeamSource::Directory(base_path) => {
                let path = format!("{base_path}/people.json");
                let data = std::fs::read_to_string(&path).map_err(|error| {
                    anyhow::anyhow!("Could not read people from a file `{path}`: {error:?}")
                })?;
                let response = serde_json::from_str::<PeopleResponse>(&data)
                    .map_err(|error| {
                        anyhow::anyhow!("Cannot deserialize people from a file '{path}': {error:?}")
                    })?
                    .into_keys()
                    .collect();
                Ok(response)
            }
        }
    }

    async fn load_team_names(&self) -> anyhow::Result<HashSet<String>> {
        match &self.team_source {
            TeamSource::Url(base_url) => {
                let url = format!("{base_url}/v1/teams.json");
                let response = reqwest::get(url)
                    .await
                    .and_then(|res| res.error_for_status())
                    .map_err(|error| anyhow::anyhow!("Cannot load teams from team API: {error:?}"))?
                    .json::<TeamResponse>()
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("Cannot deserialize teams from team API: {error:?}")
                    })?
                    .into_keys()
                    .collect();
                Ok(response)
            }
            TeamSource::Directory(base_path) => {
                let path = format!("{base_path}/teams.json");
                let data = std::fs::read_to_string(&path).map_err(|error| {
                    anyhow::anyhow!("Could not read teams from a file '{path}': {error:?}")
                })?;
                let response = serde_json::from_str::<TeamResponse>(&data)
                    .map_err(|error| {
                        anyhow::anyhow!("Cannot deserialize teams from a file '{path}': {error:?}")
                    })?
                    .into_keys()
                    .collect();
                Ok(response)
            }
        }
    }
}
