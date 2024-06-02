use std::collections::HashMap;
use std::sync::Arc;

use octocrab::Octocrab;
use parking_lot::Mutex;
use regex::Regex;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, Request, ResponseTemplate};

use crate::github::GithubRepoName;
use crate::tests::mocks::github::GitHubMockServer;
use crate::tests::mocks::permissions::TeamApiMockServer;
use crate::TeamApiClient;

pub use bors::run_test;
pub use bors::BorsBuilder;
pub use comment::Comment;
pub use permissions::Permissions;
pub use repository::default_repo_name;
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

pub struct World {
    repos: HashMap<GithubRepoName, Arc<Mutex<Repo>>>,
}

impl World {
    pub fn new() -> Self {
        Self {
            repos: Default::default(),
        }
    }

    pub fn get_repo(&mut self, name: GithubRepoName) -> Arc<Mutex<Repo>> {
        self.repos.get_mut(&name).unwrap().clone()
    }

    pub fn add_repo(mut self, repo: Repo) -> Self {
        self.repos
            .insert(repo.name.clone(), Arc::new(Mutex::new(repo)));
        self
    }
}

impl Default for World {
    fn default() -> Self {
        let repo = Repo::default();
        Self {
            repos: HashMap::from([(repo.name.clone(), Arc::new(Mutex::new(repo)))]),
        }
    }
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

/// Create a mock that dynamically responds to its requests using the given function `f`.
/// It is expected that the path will be a regex, which will be parsed when a request is received,
/// and matched capture groups will be passed as a second argument to `f`.
fn dynamic_mock_req<F: Fn(&Request, [&str; N]) -> ResponseTemplate, const N: usize>(
    f: F,
    m: &str,
    regex: String,
) -> Mock
where
    F: Send + Sync + 'static,
{
    // We need to parse the regex from the request path again, because wiremock doesn't give
    // the parsed path regex results to us :(
    let parsed_regex = Regex::new(&regex).unwrap();
    Mock::given(method(m))
        .and(path_regex(regex))
        .respond_with(move |req: &Request| {
            let captured = parsed_regex
                .captures(req.url.path())
                .unwrap()
                .extract::<N>()
                .1;
            f(req, captured)
        })
}
