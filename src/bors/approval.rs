use crate::bors::comment::approved_comment;
use crate::bors::handlers::PullRequestData;
use crate::bors::labels::handle_label_trigger;
use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::{BorsContext, Comment, RepositoryState};
use crate::database::{TreeState, WorkflowStatus};
use crate::github::LabelTrigger;
use crate::github::api::client::WorkflowSource;

/// Check if the specified approvers exist as GitHub users or teams.
pub(super) fn check_unknown_reviewers(repo: &RepositoryState, approvers: &str) -> Vec<String> {
    let directory = repo.permissions.load();

    approvers
        .split(',')
        .filter(|approver| !directory.user_exists(approver) && !directory.team_exists(approver))
        .map(str::to_string)
        .collect()
}

/// Finalize an approval and prepare the pull request for the merge queue.
///
/// Clears any failed auto build so it can be retried, wakes the merge queue, posts the approval
/// comment, and applies label changes configured for approved pull requests.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_approval(
    ctx: &BorsContext,
    repo: &RepositoryState,
    pr: PullRequestData<'_>,
    approver: &str,
    priority: Option<u32>,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<()> {
    let unknown_reviewers = check_unknown_reviewers(repo, approver);
    let was_failed = pr
        .db
        .auto_build
        .as_ref()
        .map(|b| b.status.is_failure())
        .unwrap_or(false);
    // Re-approval should act as a retry
    if was_failed {
        ctx.db.clear_auto_build(pr.db).await?;
    }

    merge_queue_tx.notify().await?;

    let mut tree_state = ctx
        .db
        .repo_db(repo.repository())
        .await?
        .map(|r| r.tree_state.clone())
        .unwrap_or(TreeState::Open);

    // If the PR has high enough priority, do not post the tree closed message
    if let TreeState::Closed {
        priority: tree_priority,
        ..
    } = &tree_state
        && let Some(priority) = priority
        && priority >= *tree_priority
    {
        tree_state = TreeState::Open;
    }

    repo.client
        .post_comment(
            pr.db.number,
            approved_comment(
                ctx.get_web_url(),
                repo.repository(),
                &pr.github.head.sha,
                approver,
                unknown_reviewers,
                tree_state,
                was_failed,
            ),
            &ctx.db,
        )
        .await?;
    handle_label_trigger(repo, &pr.github.clone().into(), LabelTrigger::Approved).await
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(super) enum TentativeApprovalOutcome {
    /// The tentative approval was confirmed or rejected.
    Resolved,
    /// Still waiting for some workflows to be finished.
    Pending,
    /// There was some transient error, the tentative approval should be resolved later.
    Skipped,
}

/// Try to resolve a tentative approval from the current PR CI state.
#[allow(clippy::too_many_arguments)]
pub(super) async fn try_resolve_tentative_approval(
    ctx: &BorsContext,
    repo: &RepositoryState,
    pr: PullRequestData<'_>,
    approver: &str,
    failure_comment: Comment,
    priority: Option<u32>,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<TentativeApprovalOutcome> {
    let workflow_runs = match repo
        .client
        .get_workflow_runs_for_commit_sha(WorkflowSource::PullRequest(pr.github))
        .await
    {
        Ok(workflow_runs) => workflow_runs,
        Err(error) => {
            tracing::error!(
                "Failed to get pull request CI status for commit {}: {error:?}",
                pr.github.head.sha
            );
            return Ok(TentativeApprovalOutcome::Skipped);
        }
    };

    if workflow_runs.is_empty() {
        return Ok(TentativeApprovalOutcome::Pending);
    }

    if workflow_runs
        .iter()
        .any(|run| run.status == WorkflowStatus::Failure)
    {
        ctx.db.unapprove(pr.db).await?;
        repo.client
            .post_comment(pr.number(), failure_comment, &ctx.db)
            .await?;
        return Ok(TentativeApprovalOutcome::Resolved);
    }

    if workflow_runs
        .iter()
        .any(|run| run.status == WorkflowStatus::Pending)
    {
        return Ok(TentativeApprovalOutcome::Pending);
    }

    ctx.db.confirm_tentative_approval(pr.db).await?;
    finalize_approval(ctx, repo, pr, approver, priority, merge_queue_tx).await?;
    Ok(TentativeApprovalOutcome::Resolved)
}
