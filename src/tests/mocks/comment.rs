use chrono::Utc;
use octocrab::models::events::payload::{IssueCommentEventAction, IssueCommentEventChanges};
use octocrab::models::issues::IssueStateReason;
use octocrab::models::{Author, CommentId, IssueId, IssueState, Label};
use serde::Serialize;
use url::Url;

use crate::github::GithubRepoName;
use crate::tests::mocks::repository::GitHubRepository;
use crate::tests::mocks::user::{GitHubUser, User};
use crate::tests::mocks::{Repo, default_pr_number, default_repo_name};

#[derive(Clone, Debug)]
pub struct Comment {
    pub repo: GithubRepoName,
    pub pr: u64,
    pub author: User,
    pub content: String,
    pub comment_id: u64,
}

impl Comment {
    pub fn new(repo: GithubRepoName, pr: u64, content: &str) -> Self {
        Self {
            repo,
            pr,
            author: User::default_pr_author(),
            content: content.to_string(),
            comment_id: 1, // comment_id will be updated by the mock server
        }
    }

    pub fn pr(pr: u64, content: &str) -> Self {
        Self::new(default_repo_name(), pr, content)
    }

    pub fn with_author(self, author: User) -> Self {
        Self { author, ..self }
    }

    pub fn with_id(self, id: u64) -> Self {
        Self {
            comment_id: id,
            ..self
        }
    }
}

impl<'a> From<&'a str> for Comment {
    fn from(value: &'a str) -> Self {
        Comment::new(Repo::default().name, default_pr_number(), value)
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
        let html_url = format!(
            "https://github.com/{}/pull/{}#issuecomment-{}",
            value.repo, value.pr, value.comment_id
        );
        let url = Url::parse(&html_url).unwrap();
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
                author_association: "OWNER".to_string(),
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
                id: CommentId(value.comment_id),
                node_id: value.comment_id.to_string(),
                url: url.clone(),
                html_url: url,
                body: Some(value.content.clone()),
                body_text: Some(value.content.clone()),
                body_html: Some(value.content.clone()),
                user: value.author.into(),
                created_at: time,
                author_association: "OWNER".to_string(),
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
    author_association: String,
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
        let html_url = format!(
            "https://github.com/{}/pull/{}#issuecomment-{}",
            value.repo, value.pr, value.comment_id
        );
        let url = Url::parse(&html_url).unwrap();
        Self {
            id: CommentId(value.comment_id),
            node_id: value.comment_id.to_string(),
            url: url.clone(),
            html_url: url,
            body: Some(value.content.clone()),
            body_text: Some(value.content.clone()),
            body_html: Some(value.content.clone()),
            user: value.author.into(),
            created_at: time,
            author_association: "OWNER".to_string(),
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
