use itertools::Itertools;
use octocrab::models::workflows::Job;
use serde::Serialize;
use std::fmt::Display;
use std::str::FromStr;
use std::time::Duration;

use crate::bors::FailedWorkflowRun;
use crate::bors::command::CommandPrefix;
use crate::github::GithubRepoName;
use crate::utils::text::pluralize;
use crate::{
    database::{WorkflowModel, WorkflowStatus},
    github::CommitSha,
};

/// A comment that can be posted to a pull request.
pub struct Comment {
    text: String,
    metadata: Option<CommentMetadata>,
}

#[derive(Serialize)]
#[serde(tag = "type")]
pub enum CommentMetadata {
    TryBuildCompleted { merge_sha: String },
}

/// A label for a comment, used to identify the comment.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CommentLabel {
    TryBuildStarted,
}

impl Display for CommentLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommentLabel::TryBuildStarted => f.write_str("TryBuildStarted"),
        }
    }
}

impl FromStr for CommentLabel {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "TryBuildStarted" => Ok(CommentLabel::TryBuildStarted),
            _ => Err(anyhow::anyhow!("Unknown comment label: {}", s)),
        }
    }
}

impl Comment {
    pub fn new(text: String) -> Self {
        Self {
            text,
            metadata: None,
        }
    }

    pub fn render(&self) -> String {
        if let Some(metadata) = &self.metadata {
            format!(
                "{}\n<!-- homu: {} -->",
                self.text,
                serde_json::to_string(metadata).unwrap()
            )
        } else {
            self.text.clone()
        }
    }
}

pub fn try_build_succeeded_comment(
    workflows: &[WorkflowModel],
    commit_sha: CommitSha,
    parent_sha: CommitSha,
) -> Comment {
    use std::fmt::Write;

    let mut text = String::from(":sunny: Try build successful");

    // If there is only a single workflow (the common case), compress the output
    // so that it doesn't take so much space
    if workflows.len() == 1 {
        writeln!(text, " ([{}]({}))", workflows[0].name, workflows[0].url).unwrap();
    } else {
        let workflows_status = list_workflows_status(workflows);
        writeln!(text, "\n{workflows_status}").unwrap();
    }
    writeln!(
        text,
        "Build commit: {commit_sha} (`{commit_sha}`, parent: `{parent_sha}`)",
    )
    .unwrap();

    Comment {
        text,
        metadata: Some(CommentMetadata::TryBuildCompleted {
            merge_sha: commit_sha.to_string(),
        }),
    }
}

pub fn cant_find_last_parent_comment() -> Comment {
    Comment::new(":exclamation: There was no previous build. Please set an explicit parent or remove the `parent=last` argument to use the default parent.".to_string())
}

pub fn no_try_build_in_progress_comment() -> Comment {
    Comment::new(":exclamation: There is currently no try build in progress.".to_string())
}

pub fn try_build_cancelled_with_failed_workflow_cancel_comment() -> Comment {
    Comment::new(
        "Try build was cancelled. It was not possible to cancel some workflows.".to_string(),
    )
}

pub fn try_build_cancelled_comment(workflow_urls: impl Iterator<Item = String>) -> Comment {
    let mut try_build_cancelled_comment =
        r#"Try build cancelled. Cancelled workflows:"#.to_string();
    for url in workflow_urls {
        try_build_cancelled_comment += format!("\n- {}", url).as_str();
    }
    Comment::new(try_build_cancelled_comment)
}

pub fn build_failed_comment(
    repo: &GithubRepoName,
    commit_sha: CommitSha,
    failed_workflows: Vec<FailedWorkflowRun>,
) -> Comment {
    use std::fmt::Write;

    let mut msg = format!(":broken_heart: Test for {commit_sha} failed");
    let mut workflow_links = failed_workflows
        .iter()
        .map(|w| format!("[{}]({})", w.workflow_run.name, w.workflow_run.url));
    if !failed_workflows.is_empty() {
        write!(msg, ": {}", workflow_links.join(", ")).unwrap();

        let mut failed_jobs: Vec<Job> = failed_workflows
            .into_iter()
            .flat_map(|w| w.failed_jobs)
            .collect();
        failed_jobs.sort_by(|l, r| l.name.cmp(&r.name));

        if !failed_jobs.is_empty() {
            write!(msg, ". Failed {}:\n\n", pluralize("job", failed_jobs.len())).unwrap();

            let max_jobs_to_show = 5;
            for job in failed_jobs.iter().take(max_jobs_to_show) {
                // Ignore this special conclusion job, as it's not very useful to show its error
                if job.name == "bors build finished" {
                    continue;
                }

                let logs_url = job.html_url.to_string();
                let extended_logs_url = format!(
                    "https://triage.rust-lang.org/gha-logs/{}/{}/{}",
                    repo.owner(),
                    repo.name(),
                    job.id
                );
                writeln!(
                    msg,
                    "- `{}` ([web logs]({}), [extended logs]({}))",
                    job.name, logs_url, extended_logs_url
                )
                .unwrap();
            }
            if failed_jobs.len() > max_jobs_to_show {
                let remaining = failed_jobs.len() - max_jobs_to_show;
                writeln!(
                    msg,
                    "- (and {remaining} other {})",
                    pluralize("job", remaining)
                )
                .unwrap();
            }
        }
    }

    Comment::new(msg)
}

pub fn try_build_started_comment(
    head_sha: &CommitSha,
    merge_sha: &CommitSha,
    bot_prefix: &CommandPrefix,
    cancelled_workflow_urls: Vec<String>,
) -> Comment {
    use std::fmt::Write;
    let mut msg = format!(":hourglass: Trying commit {head_sha} with merge {merge_sha}…\n\n");

    if !cancelled_workflow_urls.is_empty() {
        writeln!(
            msg,
            "(The previously running try build was automatically cancelled.)\n"
        )
        .unwrap();
    }

    writeln!(
        msg,
        "To cancel the try build, run the command `{bot_prefix} try cancel`."
    )
    .unwrap();

    Comment::new(msg)
}

pub fn merge_conflict_comment(branch: &str) -> Comment {
    let message = format!(
        r#":lock: Merge conflict

This pull request and the master branch diverged in a way that cannot
 be automatically merged. Please rebase on top of the latest master
 branch, and let the reviewer approve again.

<details><summary>How do I rebase?</summary>

Assuming `self` is your fork and `upstream` is this repository,
 you can resolve the conflict following these steps:

1. `git checkout {branch}` *(switch to your branch)*
2. `git fetch upstream master` *(retrieve the latest master)*
3. `git rebase upstream/master -p` *(rebase on top of it)*
4. Follow the on-screen instruction to resolve conflicts (check `git status` if you got lost).
5. `git push self {branch} --force-with-lease` *(update this PR)*

You may also read
 [*Git Rebasing to Resolve Conflicts* by Drew Blessing](http://blessing.io/git/git-rebase/open-source/2015/08/23/git-rebasing-to-resolve-conflicts.html)
 for a short tutorial.

Please avoid the ["**Resolve conflicts**" button](https://help.github.com/articles/resolving-a-merge-conflict-on-github/) on GitHub.
 It uses `git merge` instead of `git rebase` which makes the PR commit history more difficult to read.

Sometimes step 4 will complete without asking for resolution. This is usually due to difference between how `Cargo.lock` conflict is
handled during merge and rebase. This is normal, and you should still perform step 5 to update this PR.

</details>
"#
    );
    Comment::new(message)
}

pub fn approved_comment(
    web_url: &str,
    repo: &GithubRepoName,
    commit_sha: &CommitSha,
    reviewer: &str,
) -> Comment {
    Comment::new(format!(
        r":pushpin: Commit {commit_sha} has been approved by `{reviewer}`

It is now in the [queue]({web_url}/queue/{}) for this repository.
",
        repo.name()
    ))
}

pub fn approve_non_open_pr_comment() -> Comment {
    Comment::new(":clipboard: Only open, non-draft PRs can be approved.".to_string())
}

pub fn unapprove_non_open_pr_comment() -> Comment {
    Comment::new(":clipboard: Only unclosed PRs can be unapproved.".to_string())
}

pub fn approve_wip_title(keyword: &str) -> Comment {
    Comment::new(format!(
        r":clipboard: Looks like this PR is still in progress, ignoring approval.

Hint: Remove **{keyword}** from this PR's title when it is ready for review.
"
    ))
}

pub fn approve_blocking_labels_present(blocking_labels: &[&str]) -> Comment {
    let labels = blocking_labels
        .iter()
        .map(|label| format!("`{label}`"))
        .join(", ");
    Comment::new(format!(
        ":clipboard: This PR cannot be approved because it currently has the following {}: {labels}.",
        pluralize("label", blocking_labels.len())
    ))
}

pub fn delegate_try_builds_comment(delegatee: &str, bot_prefix: &CommandPrefix) -> Comment {
    Comment::new(format!(
        r":v: @{delegatee}, you can now perform try builds on this pull request!

You can now post `{bot_prefix} try` to start a try build.
    ",
    ))
}

/// `delegatee` is the user who received the delegation privileges, while `delegator` is the user
/// who gave these privileges to the `delegatee`.
pub fn delegate_comment(delegatee: &str, delegator: &str, bot_prefix: &CommandPrefix) -> Comment {
    Comment::new(format!(
        r#":v: @{delegatee}, you can now approve this pull request!

If @{delegator} told you to "`r=me`" after making some further change, then please make that change and post `{bot_prefix} r={delegator}`.
"#
    ))
}

pub fn build_timed_out_comment(timeout: Duration) -> Comment {
    Comment::new(format!(
        ":boom: Test timed out after `{}`s",
        timeout.as_secs()
    ))
}

fn list_workflows_status(workflows: &[WorkflowModel]) -> String {
    workflows
        .iter()
        .map(|w| {
            format!(
                "- [{}]({}) {}",
                w.name,
                w.url,
                match w.status {
                    WorkflowStatus::Success => ":white_check_mark:",
                    WorkflowStatus::Failure => ":x:",
                    WorkflowStatus::Pending => ":question:",
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn auto_build_started_comment(head_sha: &CommitSha, merge_sha: &CommitSha) -> Comment {
    Comment::new(format!(
        ":hourglass: Testing commit {} with merge {}...",
        head_sha, merge_sha
    ))
}

pub fn auto_build_succeeded_comment(
    workflows: &[WorkflowModel],
    approved_by: &str,
    merge_sha: &CommitSha,
    base_ref: &str,
) -> Comment {
    let urls = workflows
        .iter()
        .map(|w| format!("[{}]({})", w.name, w.url))
        .collect::<Vec<_>>()
        .join(", ");

    Comment::new(format!(
        r#":sunny: Test successful - {urls}
Approved by: `{approved_by}`
Pushing {merge_sha} to `{base_ref}`..."#
    ))
}

pub fn auto_build_push_failed_comment(error: &str) -> Comment {
    Comment::new(format!(
        ":eyes: Test was successful, but fast-forwarding failed: {error}"
    ))
}
