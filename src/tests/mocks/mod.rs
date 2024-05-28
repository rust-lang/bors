use std::collections::HashMap;

use octocrab::Octocrab;

use crate::github::GithubRepoName;
use crate::permissions::PermissionType;
use crate::tests::mocks::github::GitHubMockServer;
use crate::tests::mocks::permissions::TeamApiMockServer;
use crate::TeamApiClient;

pub use user::User;

mod app;
mod bors;
mod comment;
mod github;
mod permissions;
mod repository;
mod user;
mod webhook;

pub struct World {
    repos: HashMap<GithubRepoName, Repo>,
}

impl World {
    pub fn new() -> Self {
        Self {
            repos: Default::default(),
        }
    }

    pub fn repo(mut self, repo: Repo) -> Self {
        self.repos.insert(repo.name.clone(), repo);
        self
    }

    pub async fn build(self) -> ExternalHttpMock {
        ExternalHttpMock::start(&self).await
    }
}

impl Default for World {
    fn default() -> Self {
        Self {
            repos: HashMap::from([(default_repo_name(), Repo::default())]),
        }
    }
}

fn default_repo_name() -> GithubRepoName {
    GithubRepoName::new("rust-lang", "bors-test")
}

pub struct Repo {
    pub name: GithubRepoName,
    pub permissions: Permissions,
    pub config: String,
}

impl Repo {
    pub fn new(owner: &str, name: &str) -> Self {
        Self {
            name: GithubRepoName::new(owner, name),
            permissions: Default::default(),
            config: r#"
timeout = 3600
"#
            .to_string(),
        }
    }

    pub fn perms(mut self, user: User, permissions: &[PermissionType]) -> Self {
        self.permissions.users.insert(user, permissions.to_vec());
        self
    }
}

impl Default for Repo {
    fn default() -> Self {
        Self::new(default_repo_name().owner(), default_repo_name().name())
    }
}

#[derive(Default)]
pub struct Permissions {
    pub users: HashMap<User, Vec<PermissionType>>,
}

pub struct ExternalHttpMock {
    gh_server: GitHubMockServer,
    team_api_server: TeamApiMockServer,
}

impl ExternalHttpMock {
    pub async fn start(world: &World) -> Self {
        let gh_server = GitHubMockServer::start(world).await;
        let team_api_server = TeamApiMockServer::start(world).await;
        Self {
            gh_server,
            team_api_server,
        }
    }

    pub fn github_client(&self) -> Octocrab {
        self.gh_server.client()
    }

    pub fn team_api_client(&self) -> TeamApiClient {
        self.team_api_server.client()
    }
}
