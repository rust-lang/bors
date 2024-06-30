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
pub use bors::BorsTester;
pub use comment::Comment;
pub use permissions::Permissions;
pub use pull_request::default_pr_number;
pub use repository::default_repo_name;
pub use repository::Branch;
pub use repository::Repo;
pub use user::User;
pub use workflow::CheckSuite;
pub use workflow::TestWorkflowStatus;
pub use workflow::Workflow;
pub use workflow::WorkflowEvent;

mod app;
mod bors;
mod comment;
mod github;
mod permissions;
mod pull_request;
mod repository;
mod user;
mod workflow;

pub struct World {
    repos: HashMap<GithubRepoName, Arc<Mutex<Repo>>>,
}

impl World {
    pub fn new() -> Self {
        Self {
            repos: Default::default(),
        }
    }

    pub fn default_repo(&self) -> Arc<Mutex<Repo>> {
        self.get_repo(default_repo_name())
    }

    pub fn get_repo(&self, name: GithubRepoName) -> Arc<Mutex<Repo>> {
        self.repos.get(&name).unwrap().clone()
    }

    pub fn with_repo(mut self, repo: Repo) -> Self {
        self.repos
            .insert(repo.name.clone(), Arc::new(Mutex::new(repo)));
        self
    }

    pub fn check_sha_history(&self, repo: GithubRepoName, branch: &str, expected_shas: &[&str]) {
        let actual_shas = self
            .get_repo(repo)
            .lock()
            .get_branch_by_name(branch)
            .expect("Branch not found")
            .get_sha_history();
        let actual_shas: Vec<&str> = actual_shas.iter().map(|s| s.as_str()).collect();
        assert_eq!(actual_shas, expected_shas);
    }

    pub fn check_cancelled_workflows(&self, repo: GithubRepoName, expected_run_ids: &[u64]) {
        assert_eq!(
            &self.get_repo(repo).lock().cancelled_workflows,
            expected_run_ids
        );
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
fn dynamic_mock_req<
    F: Fn(&Request, [&str; N]) -> ResponseTemplate + Send + Sync + 'static,
    const N: usize,
>(
    f: F,
    m: &str,
    regex: String,
) -> Mock {
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
