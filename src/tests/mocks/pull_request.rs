use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

use super::{
    comment::{Comment, GitHubComment},
    Repo,
};

/// Handles all repositories related requests
/// Only handle one PR for now, since bors don't create PRs itself
#[derive(Default)]
pub(super) struct PullRequestsHandler {
    repo: Repo,
    number: u64,
    comments: Arc<Mutex<HashMap<u64, Vec<Comment>>>>,
}

impl PullRequestsHandler {
    pub(super) fn new(repo: Repo) -> Self {
        PullRequestsHandler {
            repo,
            number: default_pr_number(),
            comments: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    pub(super) async fn mount(&self, mock_server: &MockServer) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/repos/{}/pulls/{}",
                self.repo.name, self.number
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(GitHubPullRequest::default()))
            .mount(mock_server)
            .await;

        let comments = self.comments.clone();
        Mock::given(method("POST"))
            .and(path(format!(
                "/repos/{}/issues/{}/comments",
                self.repo.name, self.number
            )))
            .respond_with(move |req: &Request| {
                let comment_payload: CommentCreatePayload = req.body_json().unwrap();
                let comment: Comment = comment_payload.into();
                let mut comments = comments.lock().unwrap();
                comments.entry(1).or_default().push(comment.clone());
                ResponseTemplate::new(201).set_body_json(GitHubComment::from(comment))
            })
            .mount(mock_server)
            .await;
    }
}

#[derive(Serialize)]
struct GitHubPullRequest {
    url: String,
    id: u64,
    title: String,
    body: String,

    /// The pull request number.  Note that GitHub's REST API
    /// considers every pull-request an issue with the same number.
    number: u64,

    head: Box<Head>,
    base: Box<Base>,
}

impl Default for GitHubPullRequest {
    fn default() -> Self {
        GitHubPullRequest {
            url: "https://test.com".to_string(),
            id: 1,
            title: format!("PR #{}", default_pr_number()),
            body: "test".to_string(),
            number: default_pr_number(),
            head: Box::new(Head {
                label: "test".to_string(),
                ref_field: "test".to_string(),
                sha: "test".to_string(),
            }),
            base: Box::new(Base {
                ref_field: "test".to_string(),
                sha: "test".to_string(),
            }),
        }
    }
}

#[derive(Serialize)]
struct Head {
    label: String,
    #[serde(rename = "ref")]
    ref_field: String,
    sha: String,
}

#[derive(Serialize)]
struct Base {
    #[serde(rename = "ref")]
    ref_field: String,
    sha: String,
}

fn default_pr_number() -> u64 {
    1
}

#[derive(Deserialize)]
struct CommentCreatePayload {
    body: String,
}

impl From<CommentCreatePayload> for Comment {
    fn from(payload: CommentCreatePayload) -> Self {
        Comment::new(payload.body.as_str())
    }
}
