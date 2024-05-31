use serde::{Deserialize, Serialize};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

use super::{
    comment::{Comment, GitHubComment},
    Repo,
};

pub async fn mock_pull_requests(repo: &Repo, mock_server: &MockServer) {
    for pr_number in &repo.known_prs {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{}/pulls/{}", repo.name, pr_number)))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(GitHubPullRequest::new(*pr_number)),
            )
            .mount(mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path(format!(
                "/repos/{}/issues/{}/comments",
                repo.name, pr_number
            )))
            .respond_with(move |req: &Request| {
                let comment_payload: CommentCreatePayload = req.body_json().unwrap();
                let comment: Comment = comment_payload.into();
                // let mut comments = comments.lock().unwrap();
                // comments.entry(1).or_default().push(comment.clone());
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

impl GitHubPullRequest {
    fn new(number: u64) -> Self {
        GitHubPullRequest {
            url: "https://test.com".to_string(),
            id: number + 1000,
            title: format!("PR #{number}"),
            body: format!("Description of PR #{number}"),
            number,
            head: Box::new(Head {
                label: format!("pr-{number}"),
                ref_field: format!("pr-{number}"),
                sha: format!("pr-{number}-sha"),
            }),
            base: Box::new(Base {
                ref_field: "main".to_string(),
                sha: "main-sha".to_string(),
            }),
        }
    }
}

impl Default for GitHubPullRequest {
    fn default() -> Self {
        Self::new(default_pr_number())
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
