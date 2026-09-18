use crate::bors::comment::approved_comment;
use crate::bors::handlers::PullRequestData;
use crate::bors::labels::handle_label_trigger;
use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::{BorsContext, RepositoryState};
use crate::database::TreeState;
use crate::github::LabelTrigger;

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
