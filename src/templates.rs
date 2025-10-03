use crate::database::{MergeableState::*, PullRequestModel, TreeState};
use askama::Template;
use axum::response::{Html, IntoResponse, Response};
use http::StatusCode;

pub struct HtmlTemplate<T>(pub T);

impl<T> IntoResponse for HtmlTemplate<T>
where
    T: Template,
{
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(html) => Html(html).into_response(),
            Err(error) => {
                let message = format!("Failed to render template: {error:?}");
                tracing::error!("{message}");
                (StatusCode::INTERNAL_SERVER_ERROR, message).into_response()
            }
        }
    }
}

#[derive(Template)]
#[template(path = "help.html")]
pub struct HelpTemplate {
    pub repos: Vec<RepositoryView>,
    pub cmd_prefix: String,
}

pub struct RepositoryView {
    pub name: String,
    pub treeclosed: bool,
}

pub struct PullRequestStats {
    pub total_count: usize,
    pub in_queue_count: usize,
    pub failed_count: usize,
    pub rolled_up_count: usize,
}

#[derive(Template)]
#[template(path = "queue.html")]
pub struct QueueTemplate {
    pub repo_name: String,
    pub repo_url: String,
    pub stats: PullRequestStats,
    pub prs: Vec<PullRequestModel>,
    pub tree_state: TreeState,
}

#[derive(Template)]
#[template(path = "not_found.html")]
pub struct NotFoundTemplate {}
