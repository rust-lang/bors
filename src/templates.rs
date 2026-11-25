use crate::bors::{BuildKind, RollupMode, WorkflowJobData};
use crate::database::{
    BuildModel, MergeableState::*, PullRequestModel, QueueStatus, TreeState, WorkflowModel,
};
use crate::ec2::{Ec2Instance, Ec2InstanceStatus};
use crate::github::PullRequestNumber;
use askama::Template;
use axum::response::{Html, IntoResponse, Response};
use chrono::Utc;
use http::StatusCode;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::time::Duration;

/// Build status to display on the queue page.
fn status_text(pr: &PullRequestModel) -> &'static str {
    match pr.queue_status() {
        QueueStatus::Approved(_) => "approved",
        QueueStatus::ReadyForMerge(_, _) => "ready for merge",
        QueueStatus::Pending(_, _) => "pending",
        QueueStatus::Failed(_, _) => "failed",
        QueueStatus::TentativelyApproved(_) => "waiting for PR CI",
        QueueStatus::NotApproved | QueueStatus::NotOpen => "",
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
    pub bors_merge_queue_enabled: bool,
}

#[derive(Serialize)]
pub struct PullRequestStats {
    pub total_count: usize,
    pub in_queue_count: usize,
    pub failed_count: usize,
}

#[derive(Serialize)]
pub struct PendingWorkflowView {
    pub run_id: u64,
    pub url: String,
    pub completed_jobs: usize,
    pub total_jobs: usize,
    pub remaining_jobs: Vec<String>,
}

#[derive(Serialize)]
pub struct PendingBuildView {
    pub pr_number: u64,
    pub created_at_ms: i64,
    pub workflow: Option<PendingWorkflowView>,
}

#[derive(Serialize)]
pub struct PullRequestView {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub assignees: Vec<String>,
    pub approver: Option<String>,
    pub priority: i32,
    pub status: &'static str,
    pub mergeable: &'static str,
    pub rollup: Option<&'static str>,
    pub rollup_members: Vec<u64>,
    pub is_rollupable: bool,
    pub note: Option<String>,
}

#[derive(Serialize)]
pub struct TreeClosedInfo {
    pub priority: u32,
    pub comment_source: String,
    pub reason: Option<String>,
}

#[derive(Serialize)]
pub struct QueueData {
    pub repo_name: String,
    pub repo_owner: String,
    pub repo_url: String,
    pub tree_closed: Option<TreeClosedInfo>,
    pub ec2_configured: bool,
    pub oauth_client_id: Option<String>,
    pub stats: PullRequestStats,
    // Guesstimated duration to merge all current approved/pending PRs in the queue
    pub expected_remaining_duration_ms: Option<u64>,
    // Average build duration over the past few successful auto builds
    pub average_build_duration_ms: u64,
    // PRs that should be pre-selected for being included in a rollup
    pub selected_rollup_prs: Vec<u32>,
    // Pending auto build(s)
    pub pending_builds: Vec<PendingBuildView>,
    pub prs: Vec<PullRequestView>,
}

pub struct PendingWorkflow {
    pub workflow: WorkflowModel,
    pub jobs: Vec<WorkflowJobData>,
}

pub struct PendingBuild {
    pub build: BuildModel,
    pub workflow: Option<PendingWorkflow>,
}

#[derive(Template)]
#[template(path = "queue.html", whitespace = "minimize")]
pub struct QueueTemplate {
    pub queue_js_path: String,
    pub queue_css_path: String,
    pub data: QueueData,
}

impl TreeClosedInfo {
    pub fn from_tree_state(tree_state: TreeState) -> Option<Self> {
        match tree_state {
            TreeState::Closed {
                priority,
                source,
                reason,
            } => Some(Self {
                priority,
                comment_source: source,
                reason,
            }),
            TreeState::Open => None,
        }
    }
}

pub fn build_pr_views(
    prs: Vec<PullRequestModel>,
    rollups: &HashMap<PullRequestNumber, HashSet<PullRequestNumber>>,
) -> Vec<PullRequestView> {
    prs.into_iter()
        .map(|pr| {
            let rollup_members = rollups.get(&pr.number).map(|members| {
                let mut nums: Vec<u64> = members.iter().map(|m| m.0).collect();
                nums.sort_unstable();
                nums
            });

            let rollup = if rollup_members.is_some() {
                Some("rollup")
            } else {
                pr.rollup.as_ref().and_then(|r| match r {
                    RollupMode::Always => Some("always"),
                    RollupMode::Iffy => Some("iffy"),
                    RollupMode::Maybe => None,
                    RollupMode::Never => Some("never"),
                })
            };

            let mergeable = match pr.mergeable_status() {
                Mergeable => "yes",
                HasConflicts => "no",
                Unknown => "",
            };

            let status = status_text(&pr);
            let approver = pr.approver().map(str::to_string);
            let is_rollupable = pr.is_rollupable();
            let note = pr.note().map(str::to_string);

            PullRequestView {
                number: pr.number.0,
                title: pr.title,
                author: pr.author,
                assignees: pr.assignees,
                approver,
                priority: pr.priority.unwrap_or(0),
                status,
                mergeable,
                rollup,
                rollup_members: rollup_members.unwrap_or_default(),
                is_rollupable,
                note,
            }
        })
        .collect()
}

pub fn build_pending_build_views(
    pending_builds: HashMap<PullRequestNumber, PendingBuild>,
) -> Vec<PendingBuildView> {
    let mut pending_builds: Vec<_> = pending_builds.into_iter().collect();
    pending_builds.sort_by_key(|(pr, _)| *pr);
    pending_builds
        .into_iter()
        .map(|(pr, pending)| PendingBuildView {
            pr_number: pr.0,
            created_at_ms: pending.build.created_at.timestamp_millis(),
            workflow: pending.workflow.map(|wf| {
                let total_jobs = wf.jobs.len();
                let mut remaining_jobs: Vec<String> = wf
                    .jobs
                    .iter()
                    .filter(|j| !j.status.is_completed())
                    .map(|j| normalize_job_name(&j.name))
                    .collect();
                remaining_jobs.sort();
                let completed_jobs = total_jobs - remaining_jobs.len();

                PendingWorkflowView {
                    run_id: wf.workflow.run_id.0,
                    url: wf.workflow.url,
                    completed_jobs,
                    total_jobs,
                    remaining_jobs,
                }
            }),
        })
        .collect()
}

/// A hardcoded logic for rust-lang/rust, to strip auto/try prefixes.
fn normalize_job_name(name: &str) -> String {
    name.split_once(" - ")
        .map(|(_, rest)| rest)
        .unwrap_or(name)
        .to_string()
}

#[derive(Template)]
#[template(path = "ec2.html", whitespace = "minimize")]
pub struct EC2Template {
    pub repo_name: String,
    pub repo_owner: String,
    pub repo_url: String,
    pub instances: Vec<Ec2Instance>,
    pub loaded_at: chrono::DateTime<Utc>,
}

impl EC2Template {
    fn format_status<'a>(&self, status: &'a Ec2InstanceStatus) -> &'a str {
        match status {
            Ec2InstanceStatus::Pending => "Starting",
            Ec2InstanceStatus::Running => "Running",
            Ec2InstanceStatus::ShuttingDown => "Shutting down",
            Ec2InstanceStatus::Terminated => "Terminated",
            Ec2InstanceStatus::Stopping => "Stopping",
            Ec2InstanceStatus::Stopped => "Stopped",
            Ec2InstanceStatus::Unknown(reason) => reason.as_str(),
        }
    }

    fn get_lifetime(&self, instance: &Ec2Instance) -> String {
        let end = instance.ended_at.unwrap_or_else(Utc::now);
        let duration = end
            .signed_duration_since(instance.started_at)
            .to_std()
            .unwrap_or(Duration::ZERO);
        format_duration(duration)
    }
}

fn format_duration(duration: Duration) -> String {
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

#[derive(Template)]
#[template(path = "not_found.html")]
pub struct NotFoundTemplate {}

#[cfg(test)]
mod tests {
    use crate::templates::normalize_job_name;

    #[test]
    fn test_normalize_job() {
        assert_eq!(
            normalize_job_name("auto - aarch64-apple-macos-26"),
            "aarch64-apple-macos-26"
        );
    }
}
