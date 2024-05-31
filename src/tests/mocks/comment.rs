use chrono::Utc;
use octocrab::models::events::payload::{IssueCommentEventAction, IssueCommentEventChanges};
use octocrab::models::issues::IssueStateReason;
use octocrab::models::{Author, CommentId, IssueId, IssueState, Label};
use serde::Serialize;
use url::Url;

use crate::github::GithubRepoName;
use crate::tests::event::default_pr_number;
use crate::tests::mocks::repository::{GitHubRepository, Repo};
use crate::tests::mocks::user::{GitHubUser, User};

#[derive(Clone, Debug)]
pub struct Comment {
    repo: GithubRepoName,
    pr: u64,
    author: User,
    content: String,
}

impl Comment {
    pub fn new(content: &str) -> Self {
        Self {
            repo: Repo::default().name,
            pr: default_pr_number(),
            author: User::default(),
            content: content.to_string(),
        }
    }
}

// Copied from octocrab, since its version if #[non_exhaustive]
#[derive(Serialize)]
pub struct GitHubIssueCommentEventPayload {
    repository: GitHubRepository,
    action: IssueCommentEventAction,
    issue: GitHubIssue,
    comment: GitHubComment,
    changes: Option<IssueCommentEventChanges>,
}

impl From<Comment> for GitHubIssueCommentEventPayload {
    fn from(value: Comment) -> Self {
        let time = Utc::now();
        let url = Url::parse("https://foo.bar").unwrap();
        Self {
            repository: value.repo.into(),
            action: IssueCommentEventAction::Created,
            issue: GitHubIssue {
                id: IssueId(1),
                node_id: "1".to_string(),
                url: url.clone(),
                repository_url: url.clone(),
                labels_url: url.clone(),
                comments_url: url.clone(),
                events_url: url.clone(),
                html_url: url.clone(),
                number: value.pr,
                state: IssueState::Open,
                state_reason: None,
                title: format!("PR #{}", value.pr),
                body: None,
                body_text: None,
                body_html: None,
                user: value.author.clone().into(),
                labels: vec![],
                assignees: vec![],
                author_association: "".to_string(),
                locked: false,
                comments: 0,
                pull_request: Some(GitHubPullRequestLink {
                    url: url.clone(),
                    html_url: url.clone(),
                    diff_url: url.clone(),
                    patch_url: url.clone(),
                }),
                created_at: time,
                updated_at: time,
            },
            comment: GitHubComment {
                id: CommentId(1),
                node_id: "1".to_string(),
                url: url.clone(),
                html_url: url,
                body: Some(value.content.clone()),
                body_text: Some(value.content.clone()),
                body_html: Some(value.content.clone()),
                user: value.author.into(),
                created_at: time,
            },
            changes: None,
        }
    }
}

// Copied from octocrab, since its version if #[non_exhaustive]
#[derive(Serialize)]
struct GitHubIssue {
    id: IssueId,
    node_id: String,
    url: Url,
    repository_url: Url,
    labels_url: Url,
    comments_url: Url,
    events_url: Url,
    html_url: Url,
    number: u64,
    state: IssueState,
    state_reason: Option<IssueStateReason>,
    title: String,
    body: Option<String>,
    body_text: Option<String>,
    body_html: Option<String>,
    user: GitHubUser,
    labels: Vec<Label>,
    assignees: Vec<Author>,
    author_association: String,
    locked: bool,
    comments: u32,
    pull_request: Option<GitHubPullRequestLink>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

// Copied from octocrab, since its version if #[non_exhaustive]
#[derive(Serialize)]
pub(super) struct GitHubComment {
    id: CommentId,
    node_id: String,
    url: Url,
    html_url: Url,
    body: Option<String>,
    body_text: Option<String>,
    body_html: Option<String>,
    user: GitHubUser,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl From<Comment> for GitHubComment {
    fn from(value: Comment) -> Self {
        let time = Utc::now();
        let url = Url::parse("https://foo.bar").unwrap();
        Self {
            id: CommentId(1),
            node_id: "1".to_string(),
            url: url.clone(),
            html_url: url,
            body: Some(value.content.clone()),
            body_text: Some(value.content.clone()),
            body_html: Some(value.content.clone()),
            user: value.author.into(),
            created_at: time,
        }
    }
}

// Copied from octocrab, since its version if #[non_exhaustive]
#[derive(Serialize)]
struct GitHubPullRequestLink {
    url: Url,
    html_url: Url,
    diff_url: Url,
    patch_url: Url,
}
