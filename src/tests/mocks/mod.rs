use crate::TeamApiClient;
use crate::github::GithubRepoName;
use crate::github::api::client::HideCommentReason;
use crate::tests::Comment;
use crate::tests::mocks::github::GitHubMockServer;
use crate::tests::mocks::permissions::TeamApiMockServer;
use crate::tests::mocks::pull_request::{CommentMsg, PrIdentifier};
use crate::tests::mocks::repository::{Repo, default_repo_name};
use crate::tests::mocks::user::User;
use octocrab::Octocrab;
use parking_lot::Mutex;
use regex::Regex;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, Request, ResponseTemplate};

pub mod app;
pub mod comment;
pub mod github;
pub mod permissions;
pub mod pull_request;
pub mod repository;
pub mod user;
pub mod workflow;

#[derive(Debug)]
pub struct HiddenComment {
    pub node_id: String,
    pub reason: HideCommentReason,
}

/// Represents the state of GitHub.
pub struct GitHubState {
    pub(super) repos: HashMap<GithubRepoName, Arc<Mutex<Repo>>>,
    hidden_comments: Vec<HiddenComment>,
}

impl GitHubState {
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

    pub fn add_hidden_comment(&mut self, comment: HiddenComment) {
        self.hidden_comments.push(comment);
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
        let mut workflows = self
            .get_repo(&repo)
            .lock()
            .workflows_cancelled_by_bors
            .clone();
        workflows.sort();

        let mut expected = expected_run_ids.to_vec();
        expected.sort();

        assert_eq!(workflows, expected);
    }

    /// This function is an important synchronization point, which is used to wait for
    /// events to arrive from the bors service.
    /// As such, it has to be written carefully to avoid holding GH/repo locks that are also
    /// acquired by dynamic HTTP mock handlers.
    pub async fn get_comment<Id: Into<PrIdentifier>>(
        state: Arc<tokio::sync::Mutex<Self>>,
        id: Id,
    ) -> anyhow::Result<Comment> {
        use std::fmt::Write;

        let id = id.into();
        // We need to avoid holding the GH state and repo lock here, otherwise the mocking code
        // could not lock the repo and send the comment (or other information) to a PR.
        let comment_rx = {
            let mut gh_state = state.lock().await;
            let repo = gh_state
                .repos
                .get_mut(&id.repo)
                .unwrap_or_else(|| panic!("Repository `{}` not found", id.repo));
            let repo = repo.lock();
            let pr = repo
                .pull_requests
                .get(&id.number)
                .expect("Pull request not found");
            pr.comment_queue_rx.clone()
        };
        let mut guard = comment_rx.lock().await;

        // Timeout individual comment reads to give better error messages than if the whole test
        // times out.
        let comment = match tokio::time::timeout(Duration::from_secs(2), guard.recv()).await {
            Ok(comment) => comment,
            Err(_) => {
                let mut comment_history = String::new();

                let mut gh_state = state.lock().await;
                let repo = gh_state
                    .repos
                    .get_mut(&id.repo)
                    .unwrap_or_else(|| panic!("Repository `{}` not found", id.repo));
                let repo = repo.lock();
                let pr = repo
                    .pull_requests
                    .get(&id.number)
                    .expect("Pull request not found");
                for comment in &pr.comment_history {
                    writeln!(
                        comment_history,
                        "[{}]: {}",
                        comment.author.name, comment.content
                    )
                    .unwrap();
                }

                return Err(anyhow::anyhow!(
                    "Timed out waiting for a comment on {id}. Comment history:\n{comment_history}"
                ));
            }
        };
        let comment = comment.expect("Channel was closed while waiting for a comment");
        let comment = match comment {
            CommentMsg::Comment(comment) => comment,
            CommentMsg::Close => unreachable!(),
        };

        {
            let mut gh_state = state.lock().await;
            let repo = gh_state
                .repos
                .get_mut(&id.repo)
                .unwrap_or_else(|| panic!("Repository `{}` not found", id.repo));
            let mut repo = repo.lock();
            let pr = repo
                .pull_requests
                .get_mut(&id.number)
                .expect("Pull request not found");
            pr.add_comment_to_history(comment.clone());
        }

        eprintln!(
            "Received comment on {}#{}: {}",
            id.repo, id.number, comment.content
        );
        Ok(comment)
    }

    pub fn check_hidden_comment(&self, comment: &Comment, reason: HideCommentReason) {
        assert!(
            self.hidden_comments
                .iter()
                .any(|c| &c.node_id == comment.node_id.as_ref().unwrap() && c.reason == reason),
            "Comment {comment:?} was not hidden with reason {reason:?}.\nHidden comments: {:?}",
            self.hidden_comments
        );
    }
}

impl Default for GitHubState {
    fn default() -> Self {
        let repo = Repo::default();
        Self {
            repos: HashMap::from([(repo.name.clone(), Arc::new(Mutex::new(repo)))]),
            hidden_comments: Default::default(),
        }
    }
}

pub struct ExternalHttpMock {
    pub(super) gh_server: GitHubMockServer,
    team_api_server: TeamApiMockServer,
}

impl ExternalHttpMock {
    pub async fn start(github: Arc<tokio::sync::Mutex<GitHubState>>) -> Self {
        let gh_server = GitHubMockServer::start(github.clone()).await;
        let team_api_server = TeamApiMockServer::start(github.lock().await.deref()).await;
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
