use crate::github::api::client::HideCommentReason;
use crate::tests::mocks::User;
use crate::tests::mocks::pull_request::PrIdentifier;
use crate::tests::mocks::repository::GitHubRepository;
use crate::tests::mocks::user::GitHubUser;
use chrono::Utc;
use octocrab::models::events::payload::{IssueCommentEventAction, IssueCommentEventChanges};
use octocrab::models::issues::IssueStateReason;
use octocrab::models::{Author, CommentId, IssueId, IssueState, Label};
use serde::Serialize;
use url::Url;

#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    pub pr_ident: PrIdentifier,
    pub author: User,
    pub content: String,
    pub id: Option<u64>,
    pub node_id: Option<String>,
    pub hide_reason: Option<HideCommentReason>,
}

impl Comment {
    pub fn new<Id: Into<PrIdentifier>>(id: Id, content: &str) -> Self {
        Self {
            pr_ident: id.into(),
            author: User::default_pr_author(),
            content: content.to_string(),
            id: None,
            node_id: None,
            hide_reason: None,
        }
    }

    pub fn with_author(self, author: User) -> Self {
        Self { author, ..self }
    }

    pub fn with_ids(self, id: u64, node_id: String) -> Self {
        Self {
            id: Some(id),
            node_id: Some(node_id),
            ..self
        }
    }
}

impl<'a> From<&'a str> for Comment {
    fn from(value: &'a str) -> Self {
        Comment::new(PrIdentifier::default(), value)
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
            value.pr_ident.repo,
            value.pr_ident.number,
            value.id.unwrap()
        );
        let url = Url::parse(&html_url).unwrap();
        Self {
            repository: value.pr_ident.repo.clone().into(),
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
                number: value.pr_ident.number,
                state: IssueState::Open,
                state_reason: None,
                title: format!("PR #{}", value.pr_ident.number),
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
            comment: value.into(),
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
            value.pr_ident.repo,
            value.pr_ident.number,
            value.id.unwrap()
        );
        let url = Url::parse(&html_url).unwrap();
        Self {
            id: CommentId(value.id.unwrap()),
            node_id: value.node_id.unwrap(),
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

// Copied from octocrab, since its version is #[non_exhaustive]
#[derive(Serialize)]
struct GitHubPullRequestLink {
    url: Url,
    html_url: Url,
    diff_url: Url,
    patch_url: Url,
}
