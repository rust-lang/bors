use crate::bors::comment::{approved_comment, tentative_approval_failed_comment};
use crate::bors::handlers::PullRequestData;
use crate::bors::labels::handle_label_trigger;
use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::{BorsContext, RepositoryState};
use crate::database::{TreeState, WorkflowStatus};
use crate::github::LabelTrigger;
use crate::github::api::client::WorkflowSource;

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
    unknown_reviewers: Vec<String>,
    priority: Option<u32>,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<()> {
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

/// Returns whether the tentative approval reached a final success or failure state.
#[allow(clippy::too_many_arguments)]
pub(super) async fn resolve_tentative_approval(
    ctx: &BorsContext,
    repo: &RepositoryState,
    pr: PullRequestData<'_>,
    approver: &str,
    unknown_reviewers: Vec<String>,
    priority: Option<u32>,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<bool> {
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
            return Ok(false);
        }
    };

    if workflow_runs.is_empty() {
        return Ok(false);
    }

    if workflow_runs
        .iter()
        .any(|run| run.status == WorkflowStatus::Failure)
    {
        ctx.db.remove_tentative_approval(pr.db).await?;
        repo.client
            .post_comment(
                pr.number(),
                tentative_approval_failed_comment(&pr.github.head.sha),
                &ctx.db,
            )
            .await?;
        return Ok(true);
    }

    if workflow_runs
        .iter()
        .any(|run| run.status == WorkflowStatus::Pending)
    {
        return Ok(false);
    }

    ctx.db.promote_tentative_approval(pr.db).await?;
    finalize_approval(
        ctx,
        repo,
        pr,
        approver,
        unknown_reviewers,
        priority,
        merge_queue_tx,
    )
    .await?;
    Ok(true)
}
