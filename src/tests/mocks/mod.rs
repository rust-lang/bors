use std::collections::HashMap;

use octocrab::Octocrab;

use crate::github::GithubRepoName;
use crate::permissions::PermissionType;
use crate::tests::mocks::github::GitHubMockServer;
use crate::tests::mocks::permissions::TeamApiMockServer;
use crate::TeamApiClient;

pub use bors::run_test;
pub use bors::BorsBuilder;
pub use repository::Repo;
pub use user::User;

mod app;
mod bors;
mod comment;
mod github;
mod permissions;
mod pull_request;
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
}

impl Default for World {
    fn default() -> Self {
        let repo = Repo::default();
        Self {
            repos: HashMap::from([(repo.name.clone(), repo)]),
        }
    }
}

#[derive(Clone, Default)]
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
