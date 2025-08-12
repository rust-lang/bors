use crate::PgDbClient;
use crate::bors::event::{
    PullRequestAssigned, PullRequestClosed, PullRequestConvertedToDraft, PullRequestEdited,
    PullRequestMerged, PullRequestOpened, PullRequestPushed, PullRequestReadyForReview,
    PullRequestReopened, PullRequestUnassigned, PushToBranch,
};
use crate::bors::handlers::unapprove_pr;
use crate::bors::handlers::workflow::{AutoBuildCancelReason, maybe_cancel_auto_build};
use crate::bors::mergeable_queue::MergeableQueueSender;
use crate::bors::{Comment, PullRequestStatus, RepositoryState};
use crate::database::MergeableState;
use crate::github::{CommitSha, PullRequestNumber};
use crate::utils::text::pluralize;
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

    unapprove_pr(&repo_state, &db, &pr_model).await?;
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

    let auto_build_cancel_message = maybe_cancel_auto_build(
        &repo_state.client,
        &db,
        &pr_model,
        AutoBuildCancelReason::PushToPR,
    )
    .await?;

    if !pr_model.is_approved() {
        return Ok(());
    }

    unapprove_pr(&repo_state, &db, &pr_model).await?;
    notify_of_pushed_pr(
        &repo_state,
        pr_number,
        pr.head.sha.clone(),
        auto_build_cancel_message,
    )
    .await
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

    if !affected_prs.is_empty() {
        tracing::info!(
            "Adding {rows} {} to the mergeable queue due to a new commit pushed to base branch `{}`",
            pluralize("PR", rows as usize),
            payload.branch
        );

        for pr in affected_prs {
            mergeable_queue.enqueue(repo_state.repository().clone(), pr.number);
        }
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
        .await?;
    Ok(())
}

async fn notify_of_pushed_pr(
    repo: &RepositoryState,
    pr_number: PullRequestNumber,
    head_sha: CommitSha,
    cancel_message: Option<String>,
) -> anyhow::Result<()> {
    let mut comment = format!(
        r#":warning: A new commit `{head_sha}` was pushed to the branch, the
PR will need to be re-approved."#
    );

    if let Some(message) = cancel_message {
        comment.push_str(&format!("\n\n{message}"));
    }

    repo.client
        .post_comment(pr_number, Comment::new(comment))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};

    use crate::bors::PullRequestStatus;
    use crate::bors::merge_queue::AUTO_BUILD_CHECK_RUN_NAME;
    use crate::tests::BorsTester;
    use crate::tests::{BorsBuilder, GitHubState};
    use crate::{
        database::{MergeableState, OctocrabMergeableState},
        tests::{User, default_branch_name, default_repo_name, run_test},
    };

    fn gh_state_with_merge_queue() -> GitHubState {
        GitHubState::default().with_default_config(
            r#"
      merge_queue_enabled = true
      "#,
        )
    }

    #[sqlx::test]
    async fn unapprove_on_base_edited(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r+").await?;
            tester.expect_comments((), 1).await;
            let branch = tester.create_branch("beta").await;
            tester
                .edit_pr((), |pr| {
                    pr.base_branch = branch;
                })
                .await?;

            insta::assert_snapshot!(
                tester.get_comment(()).await?,
                @r"
            :warning: The base branch changed to `beta`, and the
            PR will need to be re-approved.
            "
            );
            tester.get_pr(()).await.expect_unapproved();
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn edit_pr_do_nothing_when_base_not_edited(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r+").await?;
            tester.expect_comments((), 1).await;
            tester.edit_pr((), |_| {}).await?;

            tester
                .get_pr(())
                .await
                .expect_approved_by(&User::default_pr_author().name);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn edit_pr_do_nothing_when_not_approved(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let branch = tester.create_branch("beta").await;
            tester
                .edit_pr((), |pr| {
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
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r+").await?;
            tester.expect_comments((), 1).await;
            tester.push_to_pr(()).await?;

            insta::assert_snapshot!(
                tester.get_comment(()).await?,
                @r"
            :warning: A new commit `pr-1-commit-1` was pushed to the branch, the
            PR will need to be re-approved.
            "
            );
            tester.get_pr(()).await.expect_unapproved();
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn push_to_pr_do_nothing_when_not_approved(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.push_to_pr(()).await?;

            // No comment should be posted
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn store_base_branch_on_pr_opened(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester
                .wait_for_pr(pr.number, |pr| {
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
        run_test(pool, async |tester: &mut BorsTester| {
            let branch = tester.create_branch("foo").await;
            tester
                .edit_pr((), |pr| {
                    pr.base_branch = branch;
                })
                .await?;
            tester.wait_for_pr((), |pr| pr.base_branch == "foo").await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn update_mergeable_state_on_pr_edited(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .edit_pr((), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Dirty;
                })
                .await?;
            tester
                .wait_for_pr((), |pr| pr.mergeable_state == MergeableState::HasConflicts)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn open_close_and_reopen_pr(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester
                .wait_for_pr(pr.number, |pr| pr.pr_status == PullRequestStatus::Open)
                .await?;
            tester.set_pr_status_closed(pr.number).await?;
            tester
                .wait_for_pr(pr.number, |pr| pr.pr_status == PullRequestStatus::Closed)
                .await?;
            tester.reopen_pr(pr.number).await?;
            tester
                .wait_for_pr(pr.number, |pr| pr.pr_status == PullRequestStatus::Open)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn open_draft_pr_and_convert_to_ready_for_review(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester
                .open_pr(default_repo_name(), |pr| {
                    pr.convert_to_draft();
                })
                .await?;
            tester
                .wait_for_pr(pr.number, |pr| pr.pr_status == PullRequestStatus::Draft)
                .await?;
            tester.set_pr_status_ready_for_review(pr.number).await?;
            tester
                .wait_for_pr(pr.number, |pr| pr.pr_status == PullRequestStatus::Open)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn open_pr_and_convert_to_draft(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester
                .wait_for_pr(pr.number, |pr| pr.pr_status == PullRequestStatus::Open)
                .await?;
            tester.set_pr_status_draft(pr.number).await?;
            tester
                .wait_for_pr(pr.number, |pr| pr.pr_status == PullRequestStatus::Draft)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn assign_pr_updates_assignees(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester
                .wait_for_pr(pr.number, |pr| pr.assignees.is_empty())
                .await?;
            tester.assign_pr(pr.number, User::reviewer()).await?;
            tester
                .wait_for_pr(pr.number, |pr| pr.assignees == vec![User::reviewer().name])
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unassign_pr_updates_assignees(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester.assign_pr(pr.number, User::reviewer()).await?;
            tester
                .wait_for_pr(pr.number, |pr| pr.assignees == vec![User::reviewer().name])
                .await?;
            tester.unassign_pr(pr.number, User::reviewer()).await?;
            tester
                .wait_for_pr(pr.number, |pr| pr.assignees.is_empty())
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn open_and_merge_pr(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester
                .wait_for_pr(pr.number, |pr| pr.pr_status == PullRequestStatus::Open)
                .await?;
            tester.set_pr_status_merged(pr.number).await?;
            tester
                .wait_for_pr(pr.number, |pr| pr.pr_status == PullRequestStatus::Merged)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn mergeable_queue_processes_pr_base_change(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let branch = tester.create_branch("beta").await;
            tester
                .edit_pr((), |pr| {
                    pr.base_branch = branch;
                    pr.mergeable_state = OctocrabMergeableState::Unknown;
                })
                .await?;
            tester
                .wait_for_pr((), |pr| pr.mergeable_state == MergeableState::Unknown)
                .await?;
            tester
                .modify_pr_state((), |pr| pr.mergeable_state = OctocrabMergeableState::Dirty)
                .await;
            tester
                .wait_for_pr((), |pr| pr.mergeable_state == MergeableState::HasConflicts)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn enqueue_prs_on_push_to_branch(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester.push_to_branch(default_branch_name()).await?;
            tester
                .wait_for_pr(pr.number, |pr| {
                    pr.mergeable_state == MergeableState::Unknown
                })
                .await?;
            tester
                .modify_pr_state(pr.number, |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Dirty
                })
                .await;
            tester
                .wait_for_pr(pr.number, |pr| {
                    pr.mergeable_state == MergeableState::HasConflicts
                })
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn enqueue_prs_on_pr_opened(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester
                .open_pr(default_repo_name(), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Dirty
                })
                .await?;
            tester
                .wait_for_pr(pr.number, |pr| {
                    pr.mergeable_state == MergeableState::HasConflicts
                })
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn enqueue_prs_on_pr_reopened(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .modify_pr_state((), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Unknown;
                })
                .await;
            tester.reopen_pr(()).await?;
            tester
                .wait_for_pr((), |pr| pr.mergeable_state == MergeableState::Unknown)
                .await?;
            tester
                .modify_pr_state((), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Dirty;
                })
                .await;
            tester
                .wait_for_pr((), |pr| pr.mergeable_state == MergeableState::HasConflicts)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn enqueue_prs_on_push_to_pr(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.push_to_pr(()).await?;
            tester
                .wait_for_pr((), |pr| pr.mergeable_state == MergeableState::Unknown)
                .await?;
            tester
                .modify_pr_state((), |pr| pr.mergeable_state = OctocrabMergeableState::Dirty)
                .await;
            tester
                .wait_for_pr((), |pr| pr.mergeable_state == MergeableState::HasConflicts)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn cancel_pending_auto_build_on_push_comment(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(gh_state_with_merge_queue())
            .run_test(async |tester: &mut BorsTester| {
                tester.post_comment("@bors r+").await?;
                tester.expect_comments((), 1).await;
                tester.process_merge_queue().await;
                tester.expect_comments((), 1).await;
                tester.wait_for_pr((), |pr| pr.auto_build.is_some()).await?;
                tester.workflow_start(tester.auto_branch().await).await?;
                tester.push_to_pr(()).await?;
                insta::assert_snapshot!(tester.get_comment(()).await?, @r"
                :warning: A new commit `pr-1-commit-1` was pushed to the branch, the
                PR will need to be re-approved.

                Auto build cancelled due to push. Cancelled workflows:

                - https://github.com/rust-lang/borstest/actions/runs/1
                ");
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn cancel_pending_auto_build_on_push_error_comment(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(gh_state_with_merge_queue())
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .modify_repo(&default_repo_name(), |repo| {
                        repo.workflow_cancel_error = true
                    })
                    .await;
                tester.post_comment("@bors r+").await?;
                tester.expect_comments((), 1).await;
                tester.process_merge_queue().await;
                tester.expect_comments((), 1).await;
                tester.wait_for_pr((), |pr| pr.auto_build.is_some()).await?;

                tester.workflow_start(tester.auto_branch().await).await?;
                tester.push_to_pr(()).await?;
                insta::assert_snapshot!(tester.get_comment(()).await?, @r"
                :warning: A new commit `pr-1-commit-1` was pushed to the branch, the
                PR will need to be re-approved.

                Auto build cancelled due to push. It was not possible to cancel some workflows.
                ");
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn cancel_pending_auto_build_on_push_updates_check_run(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(gh_state_with_merge_queue())
            .run_test(async |tester: &mut BorsTester| {
                tester.post_comment("@bors r+").await?;
                tester.expect_comments((), 1).await;
                tester.process_merge_queue().await;
                tester.expect_comments((), 1).await;
                tester.workflow_start(tester.auto_branch().await).await?;

                let prev_commit = &tester.get_pr(()).await.get_gh_pr().head_sha;
                tester.push_to_pr(()).await?;
                tester.expect_comments((), 1).await;
                tester
                    .expect_check_run(
                        prev_commit,
                        AUTO_BUILD_CHECK_RUN_NAME,
                        AUTO_BUILD_CHECK_RUN_NAME,
                        CheckRunStatus::Completed,
                        Some(CheckRunConclusion::Cancelled),
                    )
                    .await;
                Ok(())
            })
            .await;
    }
}
