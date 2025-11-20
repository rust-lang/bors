use crate::database::{
    BuildModel, BuildStatus, MergeableState::*, PullRequestModel, QueueStatus, TreeState,
};
use askama::Template;
use axum::response::{Html, IntoResponse, Response};
use chrono::{DateTime, Local, Utc};
use http::StatusCode;

/// Build status to display on the queue page.
pub fn status_text(pr: &PullRequestModel) -> String {
    if let Some(try_build) = &pr.try_build {
        try_build.status.to_string()
    } else {
        match pr.queue_status() {
            QueueStatus::Approved(_) => "approved".to_string(),
            QueueStatus::ReadyForMerge(_, _) => "ready for merge".to_string(),
            QueueStatus::Pending(_, _) => "pending".to_string(),
            QueueStatus::Stalled(_, _) => "stalled".to_string(),
            QueueStatus::NotApproved => String::new(),
        }
    }
}

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
    pub repo_owner: String,
    pub repo_url: String,
    pub stats: PullRequestStats,
    pub prs: Vec<PullRequestModel>,
    pub tree_state: TreeState,
    pub oauth_client_id: Option<String>,
}

impl QueueTemplate {
    fn to_local_time(&self, time: DateTime<Utc>) -> DateTime<Local> {
        time.into()
    }
}

#[derive(Template)]
#[template(path = "not_found.html")]
pub struct NotFoundTemplate {}

pub fn get_pending_build(pr: &PullRequestModel) -> Option<&BuildModel> {
    if let Some(auto_build) = &pr.auto_build
        && auto_build.status == BuildStatus::Pending
    {
        return Some(auto_build);
    }
    None
}
