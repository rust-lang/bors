use crate::BorsContext;
use crate::bors::approval::resolve_tentative_approval;
use crate::bors::event::WorkflowRunCompleted;
use crate::bors::handlers::PullRequestData;
use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::{PullRequestStatus, RepositoryState};
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
            let handle = async {
                let repo = ctx.get_repo(&event.repository)?;
                let pull_requests = ctx
                    .db
                    .get_nonclosed_pull_requests(&event.repository)
                    .await?;

                let matching_pull_requests = pull_requests.into_iter().filter(|pr| {
                    pr.is_tentatively_approved()
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
    let Some(approval_info) = pr.is_tentatively_approved() else {
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

    resolve_tentative_approval(
        ctx,
        repo,
        PullRequestData {
            github: &gh_pr,
            db: pr,
        },
        &approval_info.approver,
        Vec::new(),
        pr.priority.map(|priority| priority as u32),
        merge_queue_tx,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::database::WorkflowStatus;
    use crate::tests::{BorsTester, Commit, User, WorkflowEvent, run_test};

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn workflow_completion_promotes_tentative_approval_when_ci_passes(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let workflow = ctx.create_workflow((), "pr/1", "pull_request");
            ctx.approve(()).await?;

            ctx.workflow_event(WorkflowEvent::success(workflow)).await?;

            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @"
            :pushpin: Commit pr-1-sha has been approved by `default-user`

            It is now in the [queue](https://bors-test.com/queue/borstest) for this repository.
            ");
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
            let workflow = ctx.create_workflow((), "pr/1", "pull_request");
            ctx.approve(()).await?;

            ctx.workflow_event(WorkflowEvent::failure(workflow)).await?;

            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @"
            :x: Commit pr-1-sha has not been approved due to failing CI.
            ");
            ctx.pr(()).await.expect_unapproved();
            Ok(())
        })
        .await;
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn refresh_promotes_tentative_approval_when_ci_passes(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let workflow = ctx.create_workflow((), "pr/1", "pull_request");
            ctx.approve(()).await?;
            ctx.modify_workflow(workflow, |w| w.change_status(WorkflowStatus::Success));

            ctx.refresh_tentative_approvals().await;

            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @"
            :pushpin: Commit pr-1-sha has been approved by `default-user`

            It is now in the [queue](https://bors-test.com/queue/borstest) for this repository.
            ");
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
            let workflow = ctx.create_workflow((), "pr/1", "pull_request");
            ctx.approve(()).await?;
            ctx.modify_workflow(workflow, |w| w.change_status(WorkflowStatus::Failure));

            ctx.refresh_tentative_approvals().await;

            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @"
            :x: Commit pr-1-sha has not been approved due to failing CI.
            ");
            ctx.pr(()).await.expect_unapproved();
            Ok(())
        })
        .await;
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn refresh_keeps_tentative_approval_while_ci_is_pending(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.create_workflow((), "pr/1", "pull_request");
            ctx.approve(()).await?;

            ctx.refresh_tentative_approvals().await;

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
            ctx.create_workflow((), "pr/1", "pull_request");
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
            ctx.create_workflow((), "pr/1", "pull_request");
            ctx.approve(()).await?;
            ctx.modify_pr_in_gh((), |pr| pr.close());

            ctx.refresh_tentative_approvals().await;

            assert_eq!(ctx.pr(()).await.get_db_pr().approver(), None);
            Ok(())
        })
        .await;
    }
}
