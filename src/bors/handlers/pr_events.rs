use crate::PgDbClient;
use crate::bors::event::{
    PullRequestAssigned, PullRequestClosed, PullRequestConvertedToDraft, PullRequestEdited,
    PullRequestMerged, PullRequestOpened, PullRequestPushed, PullRequestReadyForReview,
    PullRequestReopened, PullRequestUnassigned, PushToBranch,
};
use crate::bors::handlers::labels::handle_label_trigger;
use crate::bors::handlers::trybuild::cancel_build_workflows;
use crate::bors::mergeable_queue::MergeableQueueSender;
use crate::bors::{Comment, PullRequestStatus, RepositoryState};
use crate::database::{BuildStatus, MergeableState};
use crate::github::{CommitSha, LabelTrigger, PullRequestNumber};
use octocrab::params::checks::CheckRunConclusion;
use std::sync::Arc;

pub(super) async fn handle_pull_request_edited(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    mergeable_queue: MergeableQueueSender,
    payload: PullRequestEdited,
) -> anyhow::Result<()> {
    let pr = &payload.pull_request;
    let pr_number = pr.number;
    let pr_model = db
        .upsert_pull_request(repo_state.repository(), pr.clone().into())
        .await?;

    // If the base branch has changed, unapprove the PR
    let Some(_) = payload.from_base_sha else {
        return Ok(());
    };

    mergeable_queue.enqueue(pr_model.repository.clone(), pr_number);

    if !pr_model.is_approved() {
        return Ok(());
    }

    db.unapprove(&pr_model).await?;
    handle_label_trigger(&repo_state, pr_number, LabelTrigger::Unapproved).await?;
    notify_of_edited_pr(&repo_state, pr_number, &payload.pull_request.base.name).await
}

pub(super) async fn handle_push_to_pull_request(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    mergeable_queue: MergeableQueueSender,
    payload: PullRequestPushed,
) -> anyhow::Result<()> {
    let pr = &payload.pull_request;
    let pr_number = pr.number;
    let pr_model = db
        .upsert_pull_request(repo_state.repository(), pr.clone().into())
        .await?;

    mergeable_queue.enqueue(repo_state.repository().clone(), pr_number);

    if let Some(auto_build) = &pr_model.auto_build {
        if auto_build.status == BuildStatus::Pending {
            tracing::info!("Cancelling auto build for PR {pr_number} due to push");

            match cancel_build_workflows(
                &repo_state.client,
                db.as_ref(),
                auto_build,
                CheckRunConclusion::Cancelled,
            )
            .await
            {
                Err(error) => {
                    tracing::error!(
                        "Could not cancel workflows for SHA {}: {error:?}",
                        auto_build.commit_sha
                    );

                    notify_of_unclean_auto_build_cancelled_comment(&repo_state, pr_number).await?
                }
                Ok(workflow_ids) => {
                    tracing::info!("Auto build cancelled");

                    let workflow_urls = repo_state
                        .client
                        .get_workflow_urls(workflow_ids.into_iter());
                    notify_of_cancelled_workflows(&repo_state, pr_number, workflow_urls).await?
                }
            };
        }

        tracing::info!(
            "Deleting auto build {} for PR {pr_number} due to push",
            auto_build.id
        );
        db.delete_auto_build(&pr_model).await?;
    }

    if !pr_model.is_approved() {
        return Ok(());
    }

    db.unapprove(&pr_model).await?;
    handle_label_trigger(&repo_state, pr_number, LabelTrigger::Unapproved).await?;
    notify_of_pushed_pr(&repo_state, pr_number, pr.head.sha.clone()).await
}

pub(super) async fn handle_pull_request_opened(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    mergeable_queue: MergeableQueueSender,
    payload: PullRequestOpened,
) -> anyhow::Result<()> {
    let pr_status = if payload.draft {
        PullRequestStatus::Draft
    } else {
        PullRequestStatus::Open
    };
    let assignees: Vec<String> = payload
        .pull_request
        .assignees
        .into_iter()
        .map(|user| user.username)
        .collect();
    db.create_pull_request(
        repo_state.repository(),
        payload.pull_request.number,
        &payload.pull_request.title,
        &payload.pull_request.author.username,
        &assignees,
        &payload.pull_request.base.name,
        pr_status,
    )
    .await?;

    mergeable_queue.enqueue(repo_state.repository().clone(), payload.pull_request.number);

    Ok(())
}

pub(super) async fn handle_pull_request_closed(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    payload: PullRequestClosed,
) -> anyhow::Result<()> {
    db.set_pr_status(
        repo_state.repository(),
        payload.pull_request.number,
        PullRequestStatus::Closed,
    )
    .await
}

pub(super) async fn handle_pull_request_merged(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    payload: PullRequestMerged,
) -> anyhow::Result<()> {
    db.set_pr_status(
        repo_state.repository(),
        payload.pull_request.number,
        PullRequestStatus::Merged,
    )
    .await
}

pub(super) async fn handle_pull_request_reopened(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    mergeable_queue: MergeableQueueSender,
    payload: PullRequestReopened,
) -> anyhow::Result<()> {
    let pr = &payload.pull_request;
    let pr_number = pr.number;
    db.upsert_pull_request(repo_state.repository(), pr.clone().into())
        .await?;

    mergeable_queue.enqueue(repo_state.repository().clone(), pr_number);

    Ok(())
}

pub(super) async fn handle_pull_request_converted_to_draft(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    payload: PullRequestConvertedToDraft,
) -> anyhow::Result<()> {
    db.set_pr_status(
        repo_state.repository(),
        payload.pull_request.number,
        PullRequestStatus::Draft,
    )
    .await
}

pub(super) async fn handle_pull_request_assigned(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    payload: PullRequestAssigned,
) -> anyhow::Result<()> {
    db.set_pr_assignees(
        repo_state.repository(),
        payload.pull_request.number,
        &payload
            .pull_request
            .assignees
            .into_iter()
            .map(|user| user.username)
            .collect::<Vec<String>>(),
    )
    .await
}

pub(super) async fn handle_pull_request_unassigned(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    payload: PullRequestUnassigned,
) -> anyhow::Result<()> {
    db.set_pr_assignees(
        repo_state.repository(),
        payload.pull_request.number,
        &payload
            .pull_request
            .assignees
            .into_iter()
            .map(|user| user.username)
            .collect::<Vec<String>>(),
    )
    .await
}

pub(super) async fn handle_pull_request_ready_for_review(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    payload: PullRequestReadyForReview,
) -> anyhow::Result<()> {
    db.set_pr_status(
        repo_state.repository(),
        payload.pull_request.number,
        PullRequestStatus::Open,
    )
    .await
}

pub(super) async fn handle_push_to_branch(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    mergeable_queue: MergeableQueueSender,
    payload: PushToBranch,
) -> anyhow::Result<()> {
    let rows = db
        .update_mergeable_states_by_base_branch(
            repo_state.repository(),
            &payload.branch,
            MergeableState::Unknown,
        )
        .await?;
    let affected_prs = db
        .get_nonclosed_pull_requests_by_base_branch(repo_state.repository(), &payload.branch)
        .await?;

    tracing::info!(
        "Adding {} PR(s) to the mergeable queue due to base branch change",
        rows
    );

    for pr in affected_prs {
        mergeable_queue.enqueue(repo_state.repository().clone(), pr.number);
    }

    Ok(())
}

async fn notify_of_edited_pr(
    repo: &RepositoryState,
    pr_number: PullRequestNumber,
    base_name: &str,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr_number,
            Comment::new(format!(
                r#":warning: The base branch changed to `{base_name}`, and the
PR will need to be re-approved."#,
            )),
        )
        .await
}

async fn notify_of_pushed_pr(
    repo: &RepositoryState,
    pr_number: PullRequestNumber,
    head_sha: CommitSha,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr_number,
            Comment::new(format!(
                r#":warning: A new commit `{}` was pushed to the branch, the
PR will need to be re-approved."#,
                head_sha
            )),
        )
        .await
}

async fn notify_of_cancelled_workflows(
    repo: &RepositoryState,
    pr_number: PullRequestNumber,
    workflow_urls: impl Iterator<Item = String>,
) -> anyhow::Result<()> {
    let mut comment =
        r#"Auto build cancelled due to push to branch. Cancelled workflows:"#.to_string();
    for url in workflow_urls {
        comment += format!("\n- {}", url).as_str();
    }

    repo.client
        .post_comment(pr_number, Comment::new(comment))
        .await
}

async fn notify_of_unclean_auto_build_cancelled_comment(
    repo: &RepositoryState,
    pr_number: PullRequestNumber,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr_number,
            Comment::new(
                "Auto build was cancelled due to push to branch. It was not possible to cancel some workflows."
                    .to_string(),
            ),
        )
        .await
}

#[cfg(test)]
mod tests {
    use crate::bors::PullRequestStatus;
    use crate::tests::mocks::default_pr_number;
    use crate::{
        database::{MergeableState, OctocrabMergeableState},
        tests::mocks::{User, default_branch_name, default_repo_name, run_test},
    };

    #[sqlx::test]
    async fn unapprove_on_base_edited(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            tester.post_comment("@bors r+").await?;
            tester.expect_comments(1).await;
            let branch = tester.create_branch("beta").clone();
            tester
                .edit_pr(default_repo_name(), default_pr_number(), |pr| {
                    pr.base_branch = branch;
                })
                .await?;

            insta::assert_snapshot!(
                tester.get_comment().await?,
                @r"
            :warning: The base branch changed to `beta`, and the
            PR will need to be re-approved.
            "
            );
            tester.default_pr().await.expect_unapproved();
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn edit_pr_do_nothing_when_base_not_edited(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            tester.post_comment("@bors r+").await?;
            tester.expect_comments(1).await;
            tester
                .edit_pr(default_repo_name(), default_pr_number(), |_| {})
                .await?;

            tester
                .default_pr()
                .await
                .expect_approved_by(&User::default_pr_author().name);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn edit_pr_do_nothing_when_not_approved(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            let branch = tester.create_branch("beta").clone();
            tester
                .edit_pr(default_repo_name(), default_pr_number(), |pr| {
                    pr.base_branch = branch;
                })
                .await?;

            // No comment should be posted
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unapprove_on_push(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            tester.post_comment("@bors r+").await?;
            tester.expect_comments(1).await;
            tester
                .push_to_pr(default_repo_name(), default_pr_number())
                .await?;

            insta::assert_snapshot!(
                tester.get_comment().await?,
                @r"
            :warning: A new commit `pr-1-commit-1` was pushed to the branch, the
            PR will need to be re-approved.
            "
            );
            tester.default_pr().await.expect_unapproved();
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn push_to_pr_do_nothing_when_not_approved(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            tester
                .push_to_pr(default_repo_name(), default_pr_number())
                .await?;

            // No comment should be posted
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn store_base_branch_on_pr_opened(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            let pr = tester.open_pr(default_repo_name(), false).await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |pr| {
                    pr.base_branch == *default_branch_name()
                        && pr.pr_status == PullRequestStatus::Open
                })
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn update_base_branch_on_pr_edited(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            let branch = tester.create_branch("foo").clone();
            tester
                .edit_pr(default_repo_name(), default_pr_number(), |pr| {
                    pr.base_branch = branch;
                })
                .await?;
            tester
                .wait_for_pr(default_repo_name(), default_pr_number(), |pr| {
                    pr.base_branch == "foo"
                })
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn update_mergeable_state_on_pr_edited(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            tester
                .edit_pr(default_repo_name(), default_pr_number(), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Dirty;
                })
                .await?;
            tester
                .wait_for_default_pr(|pr| pr.mergeable_state == MergeableState::HasConflicts)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn open_close_and_reopen_pr(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            let pr = tester.open_pr(default_repo_name(), false).await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |pr| {
                    pr.pr_status == PullRequestStatus::Open
                })
                .await?;
            tester
                .set_pr_status_closed(default_repo_name(), pr.number.0)
                .await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |pr| {
                    pr.pr_status == PullRequestStatus::Closed
                })
                .await?;
            tester.reopen_pr(default_repo_name(), pr.number.0).await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |pr| {
                    pr.pr_status == PullRequestStatus::Open
                })
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn open_draft_pr_and_convert_to_ready_for_review(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            let pr = tester.open_pr(default_repo_name(), true).await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |pr| {
                    pr.pr_status == PullRequestStatus::Draft
                })
                .await?;
            tester
                .set_pr_status_ready_for_review(default_repo_name(), pr.number.0)
                .await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |pr| {
                    pr.pr_status == PullRequestStatus::Open
                })
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn open_pr_and_convert_to_draft(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            let pr = tester.open_pr(default_repo_name(), false).await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |pr| {
                    pr.pr_status == PullRequestStatus::Open
                })
                .await?;
            tester
                .set_pr_status_draft(default_repo_name(), pr.number.0)
                .await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |pr| {
                    pr.pr_status == PullRequestStatus::Draft
                })
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn assign_pr_updates_assignees(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            let pr = tester.open_pr(default_repo_name(), false).await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |pr| {
                    pr.assignees.is_empty()
                })
                .await?;
            tester
                .assign_pr(default_repo_name(), pr.number.0, User::reviewer())
                .await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |pr| {
                    pr.assignees == vec![User::reviewer().name]
                })
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unassign_pr_updates_assignees(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            let pr = tester.open_pr(default_repo_name(), false).await?;
            tester
                .assign_pr(default_repo_name(), pr.number.0, User::reviewer())
                .await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |pr| {
                    pr.assignees == vec![User::reviewer().name]
                })
                .await?;
            tester
                .unassign_pr(default_repo_name(), pr.number.0, User::reviewer())
                .await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |pr| {
                    pr.assignees.is_empty()
                })
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn open_and_merge_pr(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            let pr = tester.open_pr(default_repo_name(), false).await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |pr| {
                    pr.pr_status == PullRequestStatus::Open
                })
                .await?;
            tester
                .set_pr_status_merged(default_repo_name(), pr.number.0)
                .await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |pr| {
                    pr.pr_status == PullRequestStatus::Merged
                })
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn mergeable_queue_processes_pr_base_change(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            let branch = tester.create_branch("beta").clone();
            tester
                .edit_pr(default_repo_name(), default_pr_number(), |pr| {
                    pr.base_branch = branch;
                    pr.mergeable_state = OctocrabMergeableState::Unknown;
                })
                .await?;
            tester
                .wait_for_default_pr(|pr| pr.mergeable_state == MergeableState::Unknown)
                .await?;
            tester
                .default_repo()
                .lock()
                .get_pr_mut(default_pr_number())
                .mergeable_state = OctocrabMergeableState::Dirty;
            tester
                .wait_for_default_pr(|pr| pr.mergeable_state == MergeableState::HasConflicts)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn enqueue_prs_on_push_to_branch(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            tester.open_pr(default_repo_name(), false).await?;
            tester.push_to_branch(default_branch_name()).await?;
            tester
                .wait_for_default_pr(|pr| pr.mergeable_state == MergeableState::Unknown)
                .await?;
            tester
                .default_repo()
                .lock()
                .get_pr_mut(default_pr_number())
                .mergeable_state = OctocrabMergeableState::Dirty;
            tester
                .wait_for_default_pr(|pr| pr.mergeable_state == MergeableState::HasConflicts)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn enqueue_prs_on_pr_opened(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            tester.open_pr(default_repo_name(), false).await?;
            tester
                .wait_for_default_pr(|pr| pr.mergeable_state == MergeableState::Unknown)
                .await?;
            tester
                .default_repo()
                .lock()
                .get_pr_mut(default_pr_number())
                .mergeable_state = OctocrabMergeableState::Dirty;
            tester
                .wait_for_default_pr(|pr| pr.mergeable_state == MergeableState::HasConflicts)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn enqueue_prs_on_pr_reopened(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            tester
                .default_repo()
                .lock()
                .get_pr_mut(default_pr_number())
                .mergeable_state = OctocrabMergeableState::Unknown;
            tester
                .reopen_pr(default_repo_name(), default_pr_number())
                .await?;
            tester
                .wait_for_default_pr(|pr| pr.mergeable_state == MergeableState::Unknown)
                .await?;
            tester
                .default_repo()
                .lock()
                .get_pr_mut(default_pr_number())
                .mergeable_state = OctocrabMergeableState::Dirty;
            tester
                .wait_for_default_pr(|pr| pr.mergeable_state == MergeableState::HasConflicts)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn enqueue_prs_on_push_to_pr(pool: sqlx::PgPool) {
        run_test(pool, async |tester| {
            tester
                .push_to_pr(default_repo_name(), default_pr_number())
                .await?;
            tester
                .wait_for_default_pr(|pr| pr.mergeable_state == MergeableState::Unknown)
                .await?;
            tester
                .default_repo()
                .lock()
                .get_pr_mut(default_pr_number())
                .mergeable_state = OctocrabMergeableState::Dirty;
            tester
                .wait_for_default_pr(|pr| pr.mergeable_state == MergeableState::HasConflicts)
                .await?;
            Ok(())
        })
        .await;
    }
}
