use std::sync::Arc;

use anyhow::Context;
use std::collections::BTreeMap;

use crate::bors::RepositoryState;
use crate::bors::mergeability_queue::MergeabilityQueueSender;
use crate::database::PullRequestModel;
use crate::github::PullRequest;
use crate::{PgDbClient, TeamApiClient};

/// Reload the team DB bors permissions for the given repository.
pub async fn reload_repository_permissions(
    repo: Arc<RepositoryState>,
    team_api_client: &TeamApiClient,
) -> anyhow::Result<()> {
    let permissions = team_api_client
        .load_permissions(repo.repository())
        .await
        .with_context(|| {
            format!(
                "Could not load permissions for repository {}",
                repo.repository()
            )
        })?;
    repo.permissions.store(Arc::new(permissions));
    Ok(())
}

/// Reloads the mergeability status from GitHub for PRs that have an unknown
/// mergeability status in the DB.
pub async fn reload_mergeability_status(
    repo: Arc<RepositoryState>,
    db: &PgDbClient,
    mergeability_queue: MergeabilityQueueSender,
) -> anyhow::Result<()> {
    let prs = db
        .get_prs_with_unknown_mergeability_state(repo.repository())
        .await?;

    tracing::info!(
        "Refreshing {} PR(s) with unknown mergeable state",
        prs.len()
    );

    for pr in prs {
        mergeability_queue.enqueue_pr(&pr, None);
    }

    Ok(())
}

/// Reloads the bors configuration for the given repository from GitHub.
pub async fn reload_repository_config(repo: Arc<RepositoryState>) -> anyhow::Result<()> {
    let config = repo.client.load_config().await?;
    repo.config.store(Arc::new(config));
    Ok(())
}

pub async fn sync_pull_requests_state(
    repo: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
) -> anyhow::Result<()> {
    tracing::debug!("Refreshing PR state from GitHub");

    let repo = repo.as_ref();
    let db = db.as_ref();
    let repo_name = repo.repository();
    // Load open/draft prs from GitHub
    let nonclosed_gh_prs = repo.client.fetch_nonclosed_pull_requests().await?;
    // Load open/draft prs from the DB
    let nonclosed_db_prs = db.get_nonclosed_pull_requests(repo_name).await?;

    let nonclosed_gh_prs_num = nonclosed_gh_prs
        .into_iter()
        .map(|pr| (pr.number, pr))
        .collect::<BTreeMap<_, _>>();

    let nonclosed_db_prs_num = nonclosed_db_prs
        .into_iter()
        .map(|pr| (pr.number, pr))
        .collect::<BTreeMap<_, _>>();

    for (pr_num, gh_pr) in &nonclosed_gh_prs_num {
        let db_pr = nonclosed_db_prs_num.get(pr_num);
        match db_pr {
            Some(db_pr) if needs_update_in_db(db_pr, gh_pr) => {
                // Nonclosed PR that needs to be updated in the DB
                tracing::debug!(
                    "PR {pr_num} has changed on GitHub, updating in DB (from {gh_pr:?} to {db_pr:?})",
                );
                db.upsert_pull_request(repo_name, gh_pr.clone().into())
                    .await?;
            }
            Some(_) => {
                // Nothing to be done here, the common case
            }
            None => {
                // Nonclosed PRs in GitHub that are either not in the DB or marked as closed
                tracing::debug!("PR {} not found in open PRs in DB, upserting it", pr_num);
                db.upsert_pull_request(repo_name, gh_pr.clone().into())
                    .await?;
            }
        }
    }
    // PRs that are closed in GitHub but not in the DB. In theory PR could also be merged
    // but bors does the merging so it should not happen.
    for pr_num in nonclosed_db_prs_num.keys() {
        if !nonclosed_gh_prs_num.contains_key(pr_num) {
            let gh_pr = repo
                .client
                .get_pull_request(*pr_num)
                .await
                .with_context(|| {
                    anyhow::anyhow!("Cannot fetch PR {}#{pr_num}", repo.repository())
                })?;
            tracing::debug!(
                "PR {pr_num} not found in open/draft prs in GitHub, marking it as {} in DB",
                gh_pr.status,
            );
            db.upsert_pull_request(repo_name, gh_pr.into()).await?;
        }
    }

    Ok(())
}

/// Returns true if the DB and GitHub representation of a PR do not match, and we need to update
/// the PR in the DB. This is an optimization to avoid writing e.g. ~1000 PRs to the DB everytime
/// we do a refresh.
fn needs_update_in_db(db_pr: &PullRequestModel, gh_pr: &PullRequest) -> bool {
    let PullRequestModel {
        id: _,
        repository: _,
        number: _,
        title: db_title,
        author: _,
        assignees: db_assignees,
        pr_status: db_status,
        head_branch: db_head_branch,
        base_branch: db_base_branch,
        mergeable_state: _,
        approval_status: _,
        delegated_permission: _,
        priority: _,
        rollup: _,
        try_build: _,
        auto_build: _,
        created_at: _,
    } = db_pr;
    let PullRequest {
        number: _,
        head_label: _,
        head,
        base,
        title,
        // It seems that when we query PRs in bulk from GitHub, the mergeability status is
        // unknown.. so we do not check it here.
        mergeable_state: _,
        message: _,
        author: _,
        assignees,
        status,
        labels: _,
        html_url: _,
    } = gh_pr;
    if status != db_status {
        return true;
    }
    if title != db_title {
        return true;
    }
    if assignees
        .iter()
        .map(|a| a.username.clone())
        .collect::<Vec<_>>()
        != *db_assignees
    {
        return true;
    }
    if head.name != *db_head_branch {
        return true;
    }
    if base.name != *db_base_branch {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use crate::bors::handlers::trybuild::TRY_BUILD_CHECK_RUN_NAME;
    use crate::bors::{PullRequestStatus, with_mocked_time};
    use crate::database::{MergeableState, OctocrabMergeableState, WorkflowStatus};
    use crate::tests::{BorsBuilder, BorsTester, GitHub, run_test};
    use crate::tests::{User, default_repo_name};
    use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};
    use std::time::Duration;

    #[sqlx::test]
    async fn refresh_no_builds(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.refresh_pending_builds().await;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_pr_state(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.refresh_prs().await;
            Ok(())
        })
        .await;
    }

    fn gh_state_with_long_timeout() -> GitHub {
        GitHub::default().with_default_config(
            r#"
timeout = 3600
"#,
        )
    }

    #[sqlx::test]
    async fn refresh_do_nothing_before_timeout(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(gh_state_with_long_timeout())
            .run_test(async |ctx: &mut BorsTester| {
                ctx.post_comment("@bors try").await?;
                ctx.expect_comments((), 1).await;
                with_mocked_time(Duration::from_secs(10), async {
                    ctx.refresh_pending_builds().await;
                })
                .await;
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn refresh_cancel_build_after_timeout(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(gh_state_with_long_timeout())
            .run_test(async |ctx: &mut BorsTester| {
                ctx.post_comment("@bors try").await?;
                ctx.expect_comments((), 1).await;
                with_mocked_time(Duration::from_secs(4000), async {
                    assert_eq!(
                        ctx
                            .db()
                            .get_pending_builds(&default_repo_name())
                            .await
                            .unwrap()
                            .len(),
                        1
                    );
                    ctx.refresh_pending_builds().await;
                })
                .await;
                insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @":boom: Test timed out after `3600`s");
                assert_eq!(
                    ctx
                        .db()
                        .get_pending_builds(&default_repo_name())
                        .await
                        .unwrap()
                        .len(),
                    0
                );
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn refresh_cancel_build_apply_timeout(pool: sqlx::PgPool) {
        let gh = GitHub::default().with_default_config(
            r#"
timeout = 3600
merge_queue_enabled = true

[labels]
auto_build_failed = ["+failed"]
"#,
        );
        run_test((pool, gh), async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            ctx.workflow_start(ctx.auto_workflow()).await?;

            with_mocked_time(Duration::from_secs(4000), async {
                ctx.refresh_pending_builds().await;
            })
                .await;
            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @":boom: Test timed out after `3600`s");
            ctx.pr(()).await.expect_added_labels(&["failed"]);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_cancel_build_updates_check_run(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(gh_state_with_long_timeout())
            .run_test(async |ctx: &mut BorsTester| {
                ctx.post_comment("@bors try").await?;
                ctx.expect_comments((), 1).await;

                with_mocked_time(Duration::from_secs(4000), async {
                    ctx.refresh_pending_builds().await;
                })
                .await;
                ctx.expect_comments((), 1).await;

                ctx.expect_check_run(
                    &ctx.pr(()).await.get_gh_pr().head_sha,
                    TRY_BUILD_CHECK_RUN_NAME,
                    "Bors try build",
                    CheckRunStatus::Completed,
                    Some(CheckRunConclusion::TimedOut),
                );

                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn refresh_cancel_workflow_after_timeout(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(gh_state_with_long_timeout())
            .run_test(async |ctx: &mut BorsTester| {
                ctx.post_comment("@bors try").await?;
                ctx.expect_comments((), 1).await;

                let run_id = ctx.try_workflow();
                ctx.workflow_start(run_id).await?;

                with_mocked_time(Duration::from_secs(4000), async {
                    ctx.refresh_pending_builds().await;
                })
                .await;
                ctx.expect_comments((), 1).await;
                ctx.expect_cancelled_workflows((), &[run_id]);
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn refresh_enqueues_unknown_mergeable_prs(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.edit_pr((), |pr| {
                pr.mergeable_state = OctocrabMergeableState::Unknown
            })
            .await?;
            ctx.wait_for_pr((), |pr| pr.mergeable_state == MergeableState::Unknown)
                .await?;
            ctx.modify_pr((), |pr| pr.mergeable_state = OctocrabMergeableState::Dirty);
            ctx.update_mergeability_status().await;
            ctx.wait_for_pr((), |pr| pr.mergeable_state == MergeableState::HasConflicts)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_new_pr(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let number = ctx.repo().lock().add_pr(User::default_pr_author()).number;
            ctx.refresh_prs().await;
            ctx.pr(number).await.expect_status(PullRequestStatus::Open);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_pr_with_status_closed(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let pr = ctx.open_pr((), |_| {}).await?;
            ctx.wait_for_pr(pr.number, |_| true).await?;

            ctx.modify_pr(pr.id(), |pr| {
                pr.close();
            });
            ctx.refresh_prs().await;
            ctx.pr(pr.number)
                .await
                .expect_status(PullRequestStatus::Closed);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_pr_with_status_draft(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let pr = ctx.open_pr((), |_| {}).await?;
            ctx.modify_pr(pr.id(), |pr| pr.convert_to_draft());

            ctx.refresh_prs().await;
            ctx.pr(pr.id())
                .await
                .expect_status(PullRequestStatus::Draft);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_pr_with_status_closed_draft(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let pr = ctx.open_pr((), |_| {}).await?;
            ctx.modify_pr(pr.id(), |pr| {
                // Both mark as closed as draft
                pr.close();
                pr.convert_to_draft();
            });

            ctx.refresh_prs().await;
            ctx.pr(pr.id())
                .await
                .expect_status(PullRequestStatus::Closed);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_pr_properties(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let pr = ctx.open_pr((), |_| {}).await?;
            let branch = ctx.create_branch("stable");
            ctx.modify_pr(pr.id(), |pr| {
                pr.title = "Foobar".to_string();
                pr.assignees = vec![User::try_user()];
                pr.base_branch = branch;
                pr.mergeable_state = octocrab::models::pulls::MergeableState::Dirty;
            });

            ctx.refresh_prs().await;
            ctx.pr(pr.id())
                .await
                .expect_mergeable_state(MergeableState::HasConflicts)
                .expect_title("Foobar")
                .expect_assignees(&[&User::try_user().name])
                .expect_base_branch("stable");
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_complete_before_timeout(pool: sqlx::PgPool) {
        run_test(
            (pool, gh_state_with_long_timeout()),
            async |ctx: &mut BorsTester| {
                ctx.post_comment("@bors try").await?;
                ctx.expect_comments((), 1).await;

                let workflow = ctx.try_workflow();
                ctx.workflow_start(workflow).await?;

                // Bors crashed and didn't start for some time
                with_mocked_time(Duration::from_secs(4000), async {
                    // Finish the workflow
                    ctx.modify_workflow(workflow, |w| {
                        w.change_status(WorkflowStatus::Success);
                    });

                    ctx.refresh_pending_builds().await;
                })
                .await;

                // The try build was completed successfully, not timed out
                insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @r#"
                :sunny: Try build successful ([Workflow1](https://github.com/rust-lang/borstest/actions/runs/1))
                Build commit: merge-0-pr-1-d7d45f1f-reauthored-to-bors (`merge-0-pr-1-d7d45f1f-reauthored-to-bors`, parent: `main-sha1`)

                <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge-0-pr-1-d7d45f1f-reauthored-to-bors"} -->
                "#);

                Ok(())
            },
        )
        .await;
    }

    #[sqlx::test]
    async fn complete_build_missed_complete_webhook(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            // Start an auto build
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            let run_id = ctx.auto_workflow();
            ctx.workflow_start(run_id).await?;

            // Now bors crashed and didn't receive a webhook about a build being completed
            ctx.modify_workflow(run_id, |w| {
                w.change_status(WorkflowStatus::Success);
            });
            ctx.refresh_pending_builds().await;
            ctx.run_merge_queue_now().await;

            // The auto build should be completed
            let comment = ctx.get_next_comment_text(()).await?;
            insta::assert_snapshot!(comment, @r#"
            :sunny: Test successful - [Workflow1](https://github.com/rust-lang/borstest/actions/runs/1)
            Approved by: `default-user`
            Pushing merge-0-pr-1-d7d45f1f-reauthored-to-bors to `main`...
            <!-- homu: {"type":"BuildCompleted","base_ref":"main","merge_sha":"merge-0-pr-1-d7d45f1f-reauthored-to-bors"} -->
            "#);

            ctx.pr(()).await.expect_status(PullRequestStatus::Merged);

            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn complete_build_missed_all_webhooks(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            // Start an auto build
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            let run_id = ctx.auto_workflow();

            // Now bors crashed and didn't receive a webhook about a build being started
            ctx.modify_workflow(run_id, |w| {
                w.change_status(WorkflowStatus::Success);
            });
            ctx.refresh_pending_builds().await;
            ctx.run_merge_queue_now().await;

            // The auto build should be completed
            let comment = ctx.get_next_comment_text(()).await?;
            insta::assert_snapshot!(comment, @r#"
            :sunny: Test successful - [Workflow1](https://github.com/rust-lang/borstest/actions/runs/1)
            Approved by: `default-user`
            Pushing merge-0-pr-1-d7d45f1f-reauthored-to-bors to `main`...
            <!-- homu: {"type":"BuildCompleted","base_ref":"main","merge_sha":"merge-0-pr-1-d7d45f1f-reauthored-to-bors"} -->
            "#);

            ctx.pr(()).await.expect_status(PullRequestStatus::Merged);

            Ok(())
        })
            .await;
    }
}
