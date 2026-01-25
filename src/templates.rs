use crate::bors::RollupMode::*;
use crate::database::{
    BuildModel, BuildStatus, MergeableState::*, PullRequestModel, QueueStatus, TreeState,
    WorkflowModel,
};
use crate::github::PullRequestNumber;
use askama::Template;
use axum::response::{Html, IntoResponse, Response};
use chrono::Utc;
use http::StatusCode;
use itertools::Itertools;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::time::Duration;

/// Build status to display on the queue page.
pub fn status_text(pr: &PullRequestModel) -> String {
    match pr.queue_status() {
        QueueStatus::Approved(_) => "approved".to_string(),
        QueueStatus::ReadyForMerge(_, _) => "ready for merge".to_string(),
        QueueStatus::Pending(_, _) => "pending".to_string(),
        QueueStatus::Failed(_, _) => "failed".to_string(),
        QueueStatus::NotApproved | QueueStatus::NotOpen => String::new(),
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
    pub help: String,
}

pub struct RepositoryView {
    pub name: String,
    pub treeclosed: bool,
}

pub struct PullRequestStats {
    pub total_count: usize,
    pub in_queue_count: usize,
    pub failed_count: usize,
}

/// A displayable view of a map of rollups to their members, meant to be used with data attributes
pub struct RollupsInfo {
    pub rollups: HashMap<PullRequestNumber, HashSet<PullRequestNumber>>,
}

impl From<HashMap<PullRequestNumber, HashSet<PullRequestNumber>>> for RollupsInfo {
    fn from(rollups: HashMap<PullRequestNumber, HashSet<PullRequestNumber>>) -> Self {
        Self { rollups }
    }
}

#[derive(Template)]
#[template(path = "queue.html", whitespace = "minimize")]
pub struct QueueTemplate {
    pub repo_name: String,
    pub repo_owner: String,
    pub repo_url: String,
    pub stats: PullRequestStats,
    pub prs: Vec<PullRequestModel>,
    pub rollups_info: RollupsInfo,
    pub tree_state: TreeState,
    pub oauth_client_id: Option<String>,
    // PRs that should be pre-selected for being included in a rollup
    pub selected_rollup_prs: Vec<u32>,
    // Active workflow for an active pending auto build
    pub pending_workflow: Option<WorkflowModel>,
    // Guesstimated duration to merge all current approved/pending PRs in the queue
    pub expected_remaining_duration: Option<Duration>,
    // Average build duration over the past few successful auto builds
    pub average_build_duration: Duration,
}

impl QueueTemplate {
    fn format_duration(&self, duration: Duration) -> String {
        let total_seconds = duration.as_secs();
        let days = total_seconds / 86400;
        let hours = (total_seconds % 86400) / 3600;
        let minutes = (total_seconds % 3600) / 60;

        let mut output = String::new();
        if days > 0 {
            write!(output, "{days}d").unwrap();
        }

        if hours > 0 {
            if !output.is_empty() {
                output.push(' ');
            }
            write!(output, "{hours}h").unwrap();
        }

        if days == 0 && minutes > 0 {
            if !output.is_empty() {
                output.push(' ');
            }
            write!(output, "{minutes}m").unwrap();
        }

        if output.is_empty() {
            output.push_str("<1m");
        }

        output
    }

    fn pending_build_elapsed(&self, build: &BuildModel) -> Duration {
        (Utc::now() - build.created_at).to_std().unwrap_or_default()
    }
}

#[derive(Template)]
#[template(path = "not_found.html")]
pub struct NotFoundTemplate {}

pub fn get_pending_auto_build(pr: &PullRequestModel) -> Option<&BuildModel> {
    if let Some(auto_build) = &pr.auto_build
        && auto_build.status == BuildStatus::Pending
    {
        return Some(auto_build);
    }
    None
}
