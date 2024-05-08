use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use url::Url;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

use super::{
    repository::Repository,
    user::{default_user, Author},
};

#[derive(Serialize)]
struct Issue {
    id: u64,
    node_id: String,
    user: Author,
    url: Url,
    pull_request: PullRequestLink,
    repository_url: Url,
    labels_url: Url,
    comments_url: Url,
    events_url: Url,
    html_url: Url,
    number: u64,
    state: IssueState,
    title: String,
    labels: Vec<Label>,
    assignees: Vec<Author>,
    author_association: String,
    locked: bool,
    comments: u32,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl Default for Issue {
    fn default() -> Self {
        Issue {
            id: 1,
            node_id: "".to_string(),
            user: default_user(),
            pull_request: PullRequestLink::default(),
            url: "https://test.com".parse().unwrap(),
            repository_url: "https://test.com".parse().unwrap(),
            labels_url: "https://test.com".parse().unwrap(),
            comments_url: "https://test.com".parse().unwrap(),
            events_url: "https://test.com".parse().unwrap(),
            html_url: "https://test.com".parse().unwrap(),
            number: default_pr_number(),
            state: IssueState::Open,
            title: "test".to_string(),
            labels: vec![Label::default()],
            assignees: vec![default_user()],
            author_association: "test".to_string(),
            locked: false,
            comments: 1,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum IssueState {
    Open,
}

#[derive(Serialize)]
struct PullRequestLink {
    url: Url,
    html_url: Url,
    diff_url: Url,
    patch_url: Url,
}

impl Default for PullRequestLink {
    fn default() -> Self {
        PullRequestLink {
            url: "https://test.com".parse().unwrap(),
            html_url: "https://test.com".parse().unwrap(),
            diff_url: "https://test.com".parse().unwrap(),
            patch_url: "https://test.com".parse().unwrap(),
        }
    }
}

#[derive(Serialize)]
struct Comment {
    id: u64,
    node_id: String,
    body: String,
    user: Author,
    url: String,
    html_url: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl Comment {
    pub(crate) fn new(comment_body: &str) -> Self {
        Self {
            id: 1,
            node_id: "".to_string(),
            body: comment_body.to_string(),
            user: default_user(),
            url: "https://test.com".to_string(),
            html_url: "https://test.com".to_string(),
            created_at: chrono::Utc::now(),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct IssueCommentEventPayload {
    action: IssueCommentEventAction,
    issue: Issue,
    comment: Comment,
    repository: Repository,
}

impl IssueCommentEventPayload {
    pub(crate) fn new(comment_body: &str) -> Self {
        Self {
            action: IssueCommentEventAction::Created,
            issue: Issue::default(),
            comment: Comment::new(comment_body),
            repository: Repository::default(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum IssueCommentEventAction {
    Created,
}

#[derive(Serialize)]
struct Label {
    id: u64,
    node_id: String,
    url: Url,
    name: String,
    color: String,
    default: bool,
}

impl Default for Label {
    fn default() -> Self {
        Label {
            id: 1,
            node_id: "".to_string(),
            url: "https://test.com".parse().unwrap(),
            name: "test".to_string(),
            color: "000000".to_string(),
            default: false,
        }
    }
}

#[derive(Serialize)]
struct PullRequest {
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

impl Default for PullRequest {
    fn default() -> Self {
        PullRequest {
            url: "https://test.com".to_string(),
            id: 1,
            title: "test".to_string(),
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

pub(super) async fn setup_pull_request_mock(mock_server: &MockServer) {
    let repo = Repository::default();
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{}/{}/pulls/{}",
            repo.owner.login.to_lowercase(),
            repo.name.to_lowercase(),
            default_pr_number()
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(PullRequest::default()))
        .mount(mock_server)
        .await;
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

pub(crate) struct InMemoryIssue {
    comments: Arc<Mutex<HashMap<u64, Vec<String>>>>,
}

impl InMemoryIssue {
    pub(crate) async fn check_comments(&self, expected_comments: &[&str]) -> bool {
        let pr_number = default_pr_number();
        // retry every 25ms until the comments are available or 1 second has passed
        for _ in 0..40 {
            {
                let comments = self.comments.lock().unwrap();
                if let Some(comments) = comments.get(&pr_number) {
                    if comments.len() == expected_comments.len() {
                        return expected_comments
                            .iter()
                            .zip(comments.iter())
                            .all(|(expected, actual)| expected == actual);
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        false
    }
}

pub(super) async fn setup_create_comment_mock(mock_server: &MockServer) -> InMemoryIssue {
    let repo = Repository::default();
    let comments = Arc::new(Mutex::new(HashMap::new()));
    let comments_clone = comments.clone();
    Mock::given(method("POST"))
        .and(path(format!(
            "/repos/{}/{}/issues/{}/comments",
            repo.owner.login.to_lowercase(),
            repo.name.to_lowercase(),
            default_pr_number()
        )))
        .respond_with(move |req: &Request| {
            let comment: CommentCreatePayload = req.body_json().unwrap();
            let mut comments = comments_clone.lock().unwrap();
            comments
                .entry(1)
                .or_insert_with(Vec::new)
                .push(comment.body.clone());
            ResponseTemplate::new(201).set_body_json(Comment::new(comment.body.as_str()))
        })
        .mount(mock_server)
        .await;
    InMemoryIssue { comments }
}

#[derive(Deserialize)]
struct CommentCreatePayload {
    body: String,
}
