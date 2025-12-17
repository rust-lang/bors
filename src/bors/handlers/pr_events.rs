use crate::PgDbClient;
use crate::bors::event::{
    PullRequestAssigned, PullRequestClosed, PullRequestComment, PullRequestConvertedToDraft,
    PullRequestEdited, PullRequestMerged, PullRequestOpened, PullRequestPushed,
    PullRequestReadyForReview, PullRequestReopened, PullRequestUnassigned, PushToBranch,
};

use crate::bors::handlers::handle_comment;
use crate::bors::handlers::unapprove_pr;
use crate::bors::handlers::workflow::{AutoBuildCancelReason, maybe_cancel_auto_build};
use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::mergeability_queue::MergeabilityQueueSender;
use crate::bors::{AUTO_BRANCH_NAME, BorsContext};
use crate::bors::{Comment, PullRequestStatus, RepositoryState};
use crate::database::{MergeableState, PullRequestModel, UpsertPullRequestParams};
use crate::github::{CommitSha, PullRequestNumber};
use crate::utils::text::pluralize;
use std::sync::Arc;

pub(super) async fn handle_pull_request_edited(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    mergeability_queue: MergeabilityQueueSender,
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

    mergeability_queue.enqueue_pr(&pr_model, None);

    if !pr_model.is_approved() {
        return Ok(());
    }

    unapprove_pr(&repo_state, &db, &pr_model).await?;
    notify_of_edited_pr(&repo_state, pr_number, &payload.pull_request.base.name).await
}

pub(super) async fn handle_push_to_pull_request(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    mergeability_queue: MergeabilityQueueSender,
    payload: PullRequestPushed,
) -> anyhow::Result<()> {
    let pr = &payload.pull_request;
    let pr_number = pr.number;
    let pr_model = db
        .upsert_pull_request(repo_state.repository(), pr.clone().into())
        .await?;

    mergeability_queue.enqueue_pr(&pr_model, None);

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

    let had_failed_build = pr_model
        .auto_build
        .as_ref()
        .map(|b| b.status.is_failure())
        .unwrap_or(false);
    unapprove_pr(&repo_state, &db, &pr_model).await?;

    // If we had an approved PR with a failed build, there's not much point in sending this warning
    if !had_failed_build {
        notify_of_pushed_pr(
            &repo_state,
            pr_number,
            pr.head.sha.clone(),
            auto_build_cancel_message,
        )
        .await
    } else {
        Ok(())
    }
}

pub(super) async fn handle_pull_request_opened(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    ctx: Arc<BorsContext>,
    mergeability_queue: MergeabilityQueueSender,
    merge_queue_tx: MergeQueueSender,
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
        .iter()
        .map(|user| user.username.clone())
        .collect();
    let pr = db
        .upsert_pull_request(
            repo_state.repository(),
            UpsertPullRequestParams {
                pr_number: payload.pull_request.number,
                title: payload.pull_request.title.clone(),
                author: payload.pull_request.author.username.clone(),
                assignees,
                base_branch: payload.pull_request.base.name.clone(),
                mergeable_state: payload.pull_request.mergeable_state.clone().into(),
                pr_status,
            },
        )
        .await?;

    process_pr_description_commands(&payload, repo_state.clone(), db, ctx, merge_queue_tx).await?;

    mergeability_queue.enqueue_pr(&pr, None);

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
    mergeability_queue: MergeabilityQueueSender,
    payload: PullRequestReopened,
) -> anyhow::Result<()> {
    let pr = &payload.pull_request;
    let pr = db
        .upsert_pull_request(repo_state.repository(), pr.clone().into())
        .await?;

    mergeability_queue.enqueue_pr(&pr, None);

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

/// Handle a push to a branch that is directly in the repo that we're managing (not in a fork).
/// This is used to handle pushes to base branches.
pub(super) async fn handle_push_to_branch(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    mergeability_queue: MergeabilityQueueSender,
    payload: PushToBranch,
) -> anyhow::Result<()> {
    let affected_prs = db
        .update_mergeable_states_by_base_branch(
            repo_state.repository(),
            &payload.branch,
            MergeableState::Unknown,
        )
        .await?;

    if !affected_prs.is_empty() {
        tracing::info!(
            "Adding {} {} to the mergeability queue due to a new commit pushed to base branch `{}`",
            affected_prs.len(),
            pluralize("PR", affected_prs.len()),
            payload.branch
        );

        // Try to find an auto build that matches this SHA
        let merged_pr = find_pr_by_merged_commit(&repo_state, &db, CommitSha(payload.sha))
            .await
            .ok()
            .flatten()
            .map(|pr| pr.number);

        for pr in affected_prs {
            mergeability_queue.enqueue_pr(&pr, merged_pr);
        }
    }

    Ok(())
}

/// Try to find a merged PR that might have produced a build with the given commit SHA.
async fn find_pr_by_merged_commit(
    repo_state: &RepositoryState,
    db: &PgDbClient,
    sha: CommitSha,
) -> anyhow::Result<Option<PullRequestModel>> {
    let Some(build) = db
        .find_build(repo_state.repository(), AUTO_BRANCH_NAME, sha)
        .await?
    else {
        return Ok(None);
    };

    let Some(pr) = db.find_pr_by_build(&build).await? else {
        return Ok(None);
    };
    if pr.auto_build.as_ref().map(|b| b.id) == Some(build.id) {
        Ok(Some(pr))
    } else {
        Ok(None)
    }
}

async fn process_pr_description_commands(
    payload: &PullRequestOpened,
    repo: Arc<RepositoryState>,
    database: Arc<PgDbClient>,
    ctx: Arc<BorsContext>,
    merge_queue_tx: MergeQueueSender,
) -> anyhow::Result<()> {
    let pr_description_comment = create_pr_description_comment(payload);
    handle_comment(repo, database, ctx, pr_description_comment, merge_queue_tx).await
}

fn create_pr_description_comment(payload: &PullRequestOpened) -> PullRequestComment {
    PullRequestComment {
        repository: payload.repository.clone(),
        author: payload.pull_request.author.clone(),
        pr_number: payload.pull_request.number,
        text: payload.pull_request.message.clone(),
        html_url: format!(
            "https://github.com/{}/pull/{}",
            payload.repository, payload.pull_request.number
        ),
    }
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
        r#":warning: A new commit `{head_sha}` was pushed to the branch, the PR will need to be re-approved."#
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
    use insta::assert_snapshot;
    use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};

    use crate::bors::PullRequestStatus;
    use crate::bors::merge_queue::AUTO_BUILD_CHECK_RUN_NAME;
    use crate::tests::default_repo_name;
    use crate::tests::{BorsBuilder, BorsTester, GitHub, WorkflowRunData};
    use crate::{
        database::{MergeableState, OctocrabMergeableState},
        tests::{User, default_branch_name, run_test},
    };

    #[sqlx::test]
    async fn unapprove_on_base_edited(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            let branch = tester.create_branch("beta");
            tester
                .edit_pr((), |pr| {
                    pr.base_branch = branch;
                })
                .await?;

            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @r"
            :warning: The base branch changed to `beta`, and the
            PR will need to be re-approved.
            "
            );
            tester.get_pr_copy(()).await.expect_unapproved();
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn edit_pr_do_nothing_when_base_not_edited(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.edit_pr((), |_| {}).await?;

            tester
                .get_pr_copy(())
                .await
                .expect_approved_by(&User::default_pr_author().name);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn edit_pr_do_nothing_when_not_approved(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let branch = tester.create_branch("beta");
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
            tester.approve(()).await?;
            tester.push_to_pr(()).await?;

            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @":warning: A new commit `pr-1-commit-1` was pushed to the branch, the PR will need to be re-approved."
            );
            tester.get_pr_copy(()).await.expect_unapproved();
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
    async fn push_to_pr_do_nothing_when_build_failed(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester.workflow_full_failure(tester.auto_branch()).await?;
            tester.expect_comments((), 1).await;
            tester.push_to_pr(()).await?;

            // No comment should be posted, but the PR should still be unapproved
            tester
                .wait_for_pr((), |pr| !pr.is_approved() && pr.auto_build.is_none())
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn store_base_branch_on_pr_opened(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr((), |_| {}).await?;
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
            let branch = tester.create_branch("foo");
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
    async fn update_mergeability_state_on_pr_edited(pool: sqlx::PgPool) {
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
            let pr = tester.open_pr((), |_| {}).await?;
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
            let pr = tester.open_pr((), |_| {}).await?;
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
            let pr = tester.open_pr((), |_| {}).await?;
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
            let pr = tester.open_pr((), |_| {}).await?;
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
            let pr = tester.open_pr((), |_| {}).await?;
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
    async fn mergeability_queue_processes_pr_base_change(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let branch = tester.create_branch("beta");
            tester
                .edit_pr((), |pr| {
                    pr.base_branch = branch;
                    pr.mergeable_state = OctocrabMergeableState::Unknown;
                })
                .await?;
            tester
                .wait_for_pr((), |pr| pr.mergeable_state == MergeableState::Unknown)
                .await?;
            tester.modify_pr_state((), |pr| pr.mergeable_state = OctocrabMergeableState::Dirty);
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
            let pr = tester
                .open_pr(default_repo_name(), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Unknown;
                })
                .await?;
            tester.push_to_branch(default_branch_name(), "sha").await?;
            tester.modify_pr_state(pr.id(), |pr| {
                pr.mergeable_state = OctocrabMergeableState::Dirty;
            });
            tester
                .wait_for_pr(pr.id(), |pr| {
                    pr.mergeable_state == MergeableState::HasConflicts
                })
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn conflict_message_disabled_in_config(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester
                .open_pr(default_repo_name(), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Clean;
                })
                .await?;
            tester.modify_pr_state(pr.id(), |pr| {
                pr.mergeable_state = OctocrabMergeableState::Dirty;
            });
            tester.push_to_branch(default_branch_name(), "sha").await?;

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn conflict_message_unknown_sha(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::default().with_default_config(
                r#"
merge_queue_enabled = true
report_merge_conflicts = true
"#,
            )).run_test(async |tester: &mut BorsTester| {
            let pr = tester
                .open_pr(default_repo_name(), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Clean;
                })
                .await?;
            tester
                .modify_pr_state(pr.id(), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Dirty;
                });
            tester.push_to_branch(default_branch_name(), "sha").await?;
            assert_snapshot!(tester.get_next_comment_text(pr).await?, @":umbrella: The latest upstream changes made this pull request unmergeable. Please [resolve the merge conflicts](https://rustc-dev-guide.rust-lang.org/git.html#rebasing-and-conflicts).");

            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn conflict_message_unknown_sha_approved(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::default().with_default_config(
                r#"
merge_queue_enabled = true
report_merge_conflicts = true
"#,
            )).run_test(async |tester: &mut BorsTester| {
            let pr = tester
                .open_pr(default_repo_name(), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Clean;
                })
                .await?;
            tester.approve(pr.id()).await?;
            tester
                .modify_pr_state(pr.id(), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Dirty;
                });
            tester.push_to_branch(default_branch_name(), "sha").await?;
            assert_snapshot!(tester.get_next_comment_text(pr.id()).await?, @r"
            :umbrella: The latest upstream changes made this pull request unmergeable. Please [resolve the merge conflicts](https://rustc-dev-guide.rust-lang.org/git.html#rebasing-and-conflicts).

            This pull request was unapproved.
            ");
            tester.get_pr_copy(pr.id()).await.expect_unapproved();

            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn conflict_message_known_sha(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::default().with_default_config(
                r#"
merge_queue_enabled = true
report_merge_conflicts = true
"#,
            )).run_test(async |tester: &mut BorsTester| {
            let pr2 = tester
                .open_pr(default_repo_name(), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Clean;
                })
                .await?;

            let pr1 = tester
                .open_pr(default_repo_name(), |_| {})
                .await?;
            tester.approve(pr1.id()).await?;
            tester.start_auto_build(pr1.id()).await?;
            tester.workflow_full_success(tester.auto_branch()).await?;
            tester.process_merge_queue().await;
            tester.expect_comments(pr1.id(), 1).await;
            let sha = tester.auto_branch().get_sha().to_string();

            tester
                .modify_pr_state(pr2.id(), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Dirty;
                });
            tester.push_to_branch(default_branch_name(), &sha).await?;
            assert_snapshot!(tester.get_next_comment_text(pr2.id()).await?, @":umbrella: The latest upstream changes (presumably #3) made this pull request unmergeable. Please [resolve the merge conflicts](https://rustc-dev-guide.rust-lang.org/git.html#rebasing-and-conflicts).");

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
            tester.modify_pr_state((), |pr| {
                pr.mergeable_state = OctocrabMergeableState::Unknown;
            });
            tester.reopen_pr(()).await?;
            tester
                .wait_for_pr((), |pr| pr.mergeable_state == MergeableState::Unknown)
                .await?;
            tester.modify_pr_state((), |pr| {
                pr.mergeable_state = OctocrabMergeableState::Dirty;
            });
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
            tester.modify_pr_state((), |pr| pr.mergeable_state = OctocrabMergeableState::Dirty);
            tester
                .wait_for_pr((), |pr| pr.mergeable_state == MergeableState::HasConflicts)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn cancel_pending_auto_build_on_push_comment(pool: sqlx::PgPool) {
        let gh = run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester.get_pr_copy(()).await.expect_auto_build(|_| true);
            tester
                .workflow_start(WorkflowRunData::from(tester.auto_branch()).with_run_id(123))
                .await?;
            tester.push_to_pr(()).await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
            :warning: A new commit `pr-1-commit-1` was pushed to the branch, the PR will need to be re-approved.

            Auto build cancelled due to push. Cancelled workflows:

            - https://github.com/rust-lang/borstest/actions/runs/123
            ");
            Ok(())
        })
            .await;
        gh.check_cancelled_workflows(default_repo_name(), &[123]);
    }

    #[sqlx::test]
    async fn cancel_pending_auto_build_on_push_error_comment(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .with_repo((), |repo| {
                    repo.workflow_cancel_error = true;
                });
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester.get_pr_copy(()).await.expect_auto_build(|_| true);

            tester.workflow_start(tester.auto_branch()).await?;
            tester.push_to_pr(()).await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
            :warning: A new commit `pr-1-commit-1` was pushed to the branch, the PR will need to be re-approved.

            Auto build cancelled due to push. It was not possible to cancel some workflows.
            ");
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn cancel_pending_auto_build_on_push_updates_check_run(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester.workflow_start(tester.auto_branch()).await?;

            let prev_commit = &tester.get_pr_copy(()).await.get_gh_pr().head_sha;
            tester.push_to_pr(()).await?;
            tester.expect_comments((), 1).await;
            tester.expect_check_run(
                prev_commit,
                AUTO_BUILD_CHECK_RUN_NAME,
                AUTO_BUILD_CHECK_RUN_NAME,
                CheckRunStatus::Completed,
                Some(CheckRunConclusion::Cancelled),
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn process_bors_commands_in_pr_description(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester
                .open_pr(default_repo_name(), |pr| {
                    pr.description = "@bors p=2".to_string();
                })
                .await?;
            tester
                .wait_for_pr(pr.number, |pr| pr.priority == Some(2))
                .await?;
            Ok(())
        })
        .await;
    }
}
