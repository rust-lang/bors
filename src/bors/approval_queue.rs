use crate::BorsContext;
use crate::bors::approval::{PrCiStatus, finalize_approval, get_pr_ci_status};
use crate::bors::comment::{
    tentative_approval_removed_comment, tentative_approval_timed_out_comment,
};
use crate::bors::event::WorkflowRunCompleted;
use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::{PullRequestStatus, RepositoryState, elapsed_time_since};
use crate::database::{ApprovalInfo, PullRequestModel};
use crate::github::{GithubRepoName, PullRequest};
use std::sync::Arc;
use tokio::sync::mpsc;

pub type ApprovalQueueReceiver = mpsc::Receiver<ApprovalQueueEvent>;

#[derive(Clone)]
pub struct ApprovalQueueSender {
    inner: mpsc::Sender<ApprovalQueueEvent>,
}

impl ApprovalQueueSender {
    pub async fn refresh_tentative_approvals(
        &self,
        repository: GithubRepoName,
    ) -> Result<(), mpsc::error::SendError<ApprovalQueueEvent>> {
        self.inner
            .send(ApprovalQueueEvent::RefreshTentativeApprovals(repository))
            .await
    }

    pub async fn on_workflow_completed(
        &self,
        event: WorkflowRunCompleted,
    ) -> Result<(), mpsc::error::SendError<ApprovalQueueEvent>> {
        #[cfg(test)]
        crate::bors::WAIT_FOR_APPROVAL_WORKFLOW_COMPLETED_HANDLED
            .drain()
            .await;

        self.inner
            .send(ApprovalQueueEvent::OnWorkflowCompleted(event))
            .await?;

        #[cfg(test)]
        crate::bors::WAIT_FOR_APPROVAL_WORKFLOW_COMPLETED_HANDLED
            .sync()
            .await;

        Ok(())
    }
}

pub fn create_approval_queue() -> (ApprovalQueueSender, ApprovalQueueReceiver) {
    let (tx, rx) = mpsc::channel(1024);
    (ApprovalQueueSender { inner: tx }, rx)
}

#[derive(Debug)]
pub enum ApprovalQueueEvent {
    RefreshTentativeApprovals(GithubRepoName),
    OnWorkflowCompleted(WorkflowRunCompleted),
}

pub async fn handle_approval_queue_event(
    ctx: Arc<BorsContext>,
    event: ApprovalQueueEvent,
    merge_queue_tx: MergeQueueSender,
) -> anyhow::Result<()> {
    match event {
        ApprovalQueueEvent::RefreshTentativeApprovals(repository) => {
            let repo = ctx.get_repo(&repository)?;
            let pull_requests = ctx.db.get_nonclosed_pull_requests(&repository).await?;

            for pr in pull_requests {
                if let Err(error) =
                    process_tentative_approval(&ctx, &repo, &pr, &merge_queue_tx).await
                {
                    tracing::error!(
                        "Failed to process tentative approval for PR #{}: {error:?}",
                        pr.number
                    );
                }
            }
        }
        ApprovalQueueEvent::OnWorkflowCompleted(event) => {
            // We have to scan all open PRs, because we sadly cannot easily get the PR number
            // from a pull_request workflow event :(
            // We could look it up in the DB via the HEAD SHA, but that seems like overkill for now.
            let handle = async {
                let repo = ctx.get_repo(&event.repository)?;
                let pull_requests = ctx
                    .db
                    .get_nonclosed_pull_requests(&event.repository)
                    .await?;

                let matching_pull_requests = pull_requests.into_iter().filter(|pr| {
                    pr.tentative_approval()
                        .is_some_and(|approval| approval.sha.as_str() == event.commit_sha.as_ref())
                });

                for pr in matching_pull_requests {
                    process_tentative_approval(&ctx, &repo, &pr, &merge_queue_tx).await?;
                }

                Ok(())
            };

            let result = handle.await;

            #[cfg(test)]
            crate::bors::WAIT_FOR_APPROVAL_WORKFLOW_COMPLETED_HANDLED.mark();

            return result;
        }
    }

    Ok(())
}

#[derive(Debug)]
enum SanityCheckError {
    /// The pull request was not open.
    WrongStatus,
    /// The pull request head SHA was different from the tentatively approved SHA.
    ApprovedShaMismatch,
}

fn sanity_check_pr(
    gh_pr: &PullRequest,
    approval_info: &ApprovalInfo,
) -> Result<(), SanityCheckError> {
    if gh_pr.status != PullRequestStatus::Open {
        return Err(SanityCheckError::WrongStatus);
    }

    if gh_pr.head.sha.as_ref() != approval_info.sha {
        return Err(SanityCheckError::ApprovedShaMismatch);
    }

    Ok(())
}

async fn process_tentative_approval(
    ctx: &BorsContext,
    repo: &RepositoryState,
    pr: &PullRequestModel,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<()> {
    let Some(approval_info) = pr.tentative_approval() else {
        return Ok(());
    };

    let pr_number = pr.number;
    let gh_pr = repo.client.get_pull_request(pr_number).await?;

    if let Err(error) = sanity_check_pr(&gh_pr, approval_info) {
        tracing::info!(
            "Removing tentative approval for PR #{pr_number} after sanity check failed: {error:?}"
        );
        ctx.db.unapprove(pr).await?;
        return Ok(());
    }

    let pr_ci_status = get_pr_ci_status(repo, &gh_pr).await?;
    match pr_ci_status {
        PrCiStatus::Pending => {
            let Some(head_update_time) = repo
                .client
                .get_pull_request_head_update_time(&gh_pr)
                .await?
            else {
                tracing::warn!(
                    "Could not find the last head update for PR #{} at commit {}",
                    pr.number,
                    gh_pr.head.sha
                );
                return Ok(());
            };

            let timeout = repo.config.load().pr_ci_timeout;

            // CI has timed out
            if elapsed_time_since(head_update_time) >= timeout {
                ctx.db.unapprove(pr).await?;
                repo.client
                    .post_comment(
                        pr.number,
                        tentative_approval_timed_out_comment(&gh_pr.head.sha, timeout),
                        &ctx.db,
                    )
                    .await?;
            }
        }
        PrCiStatus::Success => {
            // CI is green! Confirm the approval
            ctx.db.confirm_tentative_approval(pr).await?;
            finalize_approval(repo, &gh_pr, merge_queue_tx).await?;
        }
        PrCiStatus::Failed => {
            // CI has failed
            ctx.db.unapprove(pr).await?;
            repo.client
                .post_comment(
                    pr.number,
                    tentative_approval_removed_comment(&gh_pr.head.sha),
                    &ctx.db,
                )
                .await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::bors::with_mocked_time;
    use crate::database::WorkflowStatus;
    use crate::tests::{BorsTester, Commit, GitHub, User, run_test};
    use std::time::Duration;

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn workflow_completion_promotes_tentative_approval_when_ci_passes(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let workflow = ctx.pr_ci_workflow(());
            ctx.approve(()).await?;

            ctx.pr(())
                .await
                .expect_unapproved()
                .expect_tentative_approval();

            ctx.pr_workflow_success(workflow).await?;
            ctx.pr(())
                .await
                .expect_approved_by(&User::default_pr_author().name);
            Ok(())
        })
        .await;
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn workflow_completion_rejects_tentative_approval_when_ci_fails(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let workflow = ctx.pr_ci_workflow(());
            ctx.approve(()).await?;

            ctx.pr_workflow_failure(workflow).await?;

            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @":x: Commit pr-1-sha has been unapproved due to PR CI failure. Reapprove it with `@bors r+ force` if you want to ignore the failure.");
            ctx.pr(()).await.expect_unapproved();
            Ok(())
        })
        .await;
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn refresh_promotes_tentative_approval_when_ci_passes(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let workflow = ctx.pr_ci_workflow(());
            ctx.approve(()).await?;
            // Missed PR CI completion webhook
            ctx.modify_workflow(workflow, |w| w.change_status(WorkflowStatus::Success));

            ctx.refresh_tentative_approvals().await;

            ctx.pr(())
                .await
                .expect_approved_by(&User::default_pr_author().name);
            Ok(())
        })
        .await;
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn refresh_rejects_tentative_approval_when_ci_fails(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let workflow = ctx.pr_ci_workflow(());
            ctx.approve(()).await?;
            // Missed PR CI completion webhook
            ctx.modify_workflow(workflow, |w| w.change_status(WorkflowStatus::Failure));

            ctx.refresh_tentative_approvals().await;

            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @":x: Commit pr-1-sha has been unapproved due to PR CI failure. Reapprove it with `@bors r+ force` if you want to ignore the failure.");
            ctx.pr(()).await.expect_unapproved();
            Ok(())
        })
        .await;
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn refresh_keeps_tentative_approval_while_ci_is_pending(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.pr_ci_workflow(());
            ctx.approve(()).await?;

            ctx.refresh_tentative_approvals().await;

            ctx.pr(())
                .await
                .expect_approver(&User::default_pr_author().name)
                .expect_tentative_approval();
            Ok(())
        })
        .await;
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn refresh_rejects_tentative_approval_when_ci_does_not_start(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.modify_repo((), |repo| repo.default_pr_ci = false);
            ctx.approve(()).await?;

            with_mocked_time(Duration::from_secs(8000), async {
                ctx.refresh_tentative_approvals().await;
            })
            .await;

            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @":x: Commit pr-1-sha has been unapproved because PR CI timed out after `7200s`.");
            ctx.pr(()).await.expect_unapproved();
            Ok(())
        })
        .await;
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn refresh_rejects_approval_on_ci_timeout(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.pr_ci_workflow(());
            ctx.approve(()).await?;

            with_mocked_time(Duration::from_secs(8000), async {
                ctx.refresh_tentative_approvals().await;
            })
            .await;

            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @":x: Commit pr-1-sha has been unapproved because PR CI timed out after `7200s`.");
            ctx.pr(()).await.expect_unapproved();
            Ok(())
        })
        .await;
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn refresh_confirms_approval_before_timeout(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let workflow = ctx.pr_ci_workflow(());
            ctx.approve(()).await?;
            // Missed PR CI completion webhook
            ctx.modify_workflow(workflow, |w| w.change_status(WorkflowStatus::Success));

            with_mocked_time(Duration::from_secs(4000), async {
                ctx.refresh_tentative_approvals().await;
            })
            .await;

            ctx.pr(())
                .await
                .expect_approved_by(&User::default_pr_author().name);
            Ok(())
        })
        .await;
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn refresh_removes_tentative_approval_after_head_changes(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.pr_ci_workflow(());
            ctx.approve(()).await?;
            ctx.modify_pr_in_gh((), |pr| {
                pr.add_commits(vec![Commit::from_sha("new-head-sha")])
            });

            ctx.refresh_tentative_approvals().await;

            assert_eq!(ctx.pr(()).await.get_db_pr().approver(), None);
            Ok(())
        })
        .await;
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn refresh_removes_tentative_approval_when_pr_is_closed(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.pr_ci_workflow(());
            ctx.approve(()).await?;
            ctx.modify_pr_in_gh((), |pr| pr.close());

            ctx.refresh_tentative_approvals().await;

            assert_eq!(ctx.pr(()).await.get_db_pr().approver(), None);
            Ok(())
        })
        .await;
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn approval_confirmation_adds_labels(pool: sqlx::PgPool) {
        let gh = GitHub::default().append_to_default_config(
            r#"
[labels]
approved = ["+approved"]
"#,
        );
        run_test((pool, gh), async |ctx: &mut BorsTester| {
            let workflow = ctx.pr_ci_workflow(());
            ctx.approve(()).await?;
            ctx.pr_workflow_success(workflow).await?;
            ctx.pr(()).await.expect_added_labels(&["approved"]);
            Ok(())
        })
        .await;
    }
}
