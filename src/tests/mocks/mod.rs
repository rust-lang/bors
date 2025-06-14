use std::collections::HashMap;
use std::sync::Arc;

use octocrab::Octocrab;
use parking_lot::Mutex;
use regex::Regex;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, Request, ResponseTemplate};

use crate::TeamApiClient;
use crate::github::GithubRepoName;
use crate::tests::mocks::github::GitHubMockServer;
use crate::tests::mocks::permissions::TeamApiMockServer;

pub use bors::BorsBuilder;
pub use bors::default_cmd_prefix;
pub use bors::run_test;
pub use comment::Comment;
pub use permissions::Permissions;
pub use pull_request::default_pr_number;
pub use repository::Branch;
pub use repository::Repo;
pub use repository::default_branch_name;
pub use repository::default_repo_name;
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

/// Represents the state of GitHub.
pub struct GitHubState {
    repos: HashMap<GithubRepoName, Arc<Mutex<Repo>>>,
}

impl GitHubState {
    pub fn new() -> Self {
        Self {
            repos: Default::default(),
        }
    }

    /// Creates a new GitHubState where the default PR author has no permissions.
    pub fn unauthorized_pr_author() -> Self {
        let state = Self::default();
        state
            .default_repo()
            .lock()
            .permissions
            .users
            .insert(User::default_pr_author(), vec![]);
        state
    }

    pub fn with_default_config(self, config: &str) -> Self {
        self.default_repo().lock().config = config.to_string();
        self
    }

    pub fn default_repo(&self) -> Arc<Mutex<Repo>> {
        self.get_repo(&default_repo_name())
    }

    pub fn get_repo(&self, name: &GithubRepoName) -> Arc<Mutex<Repo>> {
        self.repos.get(name).unwrap().clone()
    }

    pub fn with_repo(mut self, repo: Repo) -> Self {
        self.repos
            .insert(repo.name.clone(), Arc::new(Mutex::new(repo)));
        self
    }

    pub fn check_sha_history(&self, repo: GithubRepoName, branch: &str, expected_shas: &[&str]) {
        let actual_shas = self
            .get_repo(&repo)
            .lock()
            .get_branch_by_name(branch)
            .expect("Branch not found")
            .get_sha_history();
        let actual_shas: Vec<&str> = actual_shas.iter().map(|s| s.as_str()).collect();
        assert_eq!(actual_shas, expected_shas);
    }

    pub fn check_cancelled_workflows(&self, repo: GithubRepoName, expected_run_ids: &[u64]) {
        let mut workflows = self.get_repo(&repo).lock().cancelled_workflows.clone();
        workflows.sort();

        let mut expected = expected_run_ids.to_vec();
        expected.sort();

        assert_eq!(workflows, expected);
    }
}

impl Default for GitHubState {
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
    pub async fn start(github: &GitHubState) -> Self {
        let gh_server = GitHubMockServer::start(github).await;
        let team_api_server = TeamApiMockServer::start(github).await;
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
