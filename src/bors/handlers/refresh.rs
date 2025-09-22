use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use octocrab::params::checks::CheckRunConclusion;
use std::collections::BTreeMap;

use crate::bors::PullRequestStatus;
use crate::bors::RepositoryState;
use crate::bors::comment::build_timed_out_comment;
use crate::bors::handlers::workflow::{CancelBuildError, cancel_build};
use crate::bors::mergeability_queue::MergeabilityQueueSender;
use crate::database::{BuildModel, BuildStatus};
use crate::{PgDbClient, TeamApiClient};

/// Go through pending builds and figure out if we need to do something about them:
/// - Cancel CI builds that have been running for too long.
pub async fn refresh_pending_builds(
    repo: Arc<RepositoryState>,
    db: &PgDbClient,
) -> anyhow::Result<()> {
    let running_builds = db.get_pending_builds(repo.repository()).await?;
    tracing::info!("Found {} pending build(s)", running_builds.len());

    let timeout = repo.config.load().timeout;
    for build in running_builds {
        if let Err(error) = refresh_build(&repo, db, &build, timeout).await {
            tracing::error!("Could not refresh pending build {build:?}: {error:?}");
        }
    }
    Ok(())
}

async fn refresh_build(
    repo: &RepositoryState,
    db: &PgDbClient,
    build: &BuildModel,
    timeout: Duration,
) -> anyhow::Result<()> {
    if elapsed_time(build.created_at) >= timeout {
        if let Some(pr) = db.find_pr_by_build(build).await? {
            tracing::info!("Cancelling build {build:?}");
            match cancel_build(&repo.client, db, build, CheckRunConclusion::TimedOut).await {
                Ok(_) => {}
                Err(
                    CancelBuildError::FailedToMarkBuildAsCancelled(error)
                    | CancelBuildError::FailedToCancelWorkflows(error),
                ) => {
                    tracing::error!(
                        "Could not cancel workflows for SHA {}: {error:?}",
                        build.commit_sha
                    );
                }
            }

            if let Err(error) = repo
                .client
                .post_comment(pr.number, build_timed_out_comment(timeout))
                .await
            {
                tracing::error!("Could not send comment to PR {}: {error:?}", pr.number);
            }
        } else {
            // This is an orphaned build. It should never be created, unless we have some bug or
            // unexpected race condition in bors.
            // When we do encounter such a build, we can mark it as timeouted, as it is no longer
            // relevant.
            // Note that we could write an explicit query for finding these orphaned builds,
            // but that could be quite expensive. Instead we piggyback on the existing logic
            // for timed out builds; if a build is still pending and has no PR attached, then
            // there likely won't be any additional event that could mark it as finished.
            // So eventually all such builds will arrive here
            tracing::warn!(
                "Detected orphaned pending without a PR, marking it as time outed: {build:?}"
            );
            db.update_build_status(build, BuildStatus::Timeouted)
                .await?;
        }
    }
    Ok(())
}

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
        mergeability_queue.enqueue_pr(repo.repository().clone(), pr.number);
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
        if let Some(db_pr) = db_pr {
            if db_pr.pr_status != gh_pr.status {
                // PR status changed in GitHub
                tracing::debug!(
                    "PR {} status changed from {:?} to {:?}",
                    pr_num,
                    db_pr.pr_status,
                    gh_pr.status
                );
                db.set_pr_status(repo_name, *pr_num, gh_pr.status).await?;
            }
        } else {
            // Nonclosed PRs in GitHub that are either not in the DB or marked as closed
            tracing::debug!("PR {} not found in open PRs in DB, upserting it", pr_num);
            db.upsert_pull_request(repo_name, gh_pr.clone().into())
                .await?;
        }
    }
    // PRs that are closed in GitHub but not in the DB. In theory PR could also be merged
    // but bors does the merging so it should not happen.
    for pr_num in nonclosed_db_prs_num.keys() {
        if !nonclosed_gh_prs_num.contains_key(pr_num) {
            tracing::debug!(
                "PR {} not found in open/draft prs in GitHub, closing it in DB",
                pr_num
            );
            db.set_pr_status(repo_name, *pr_num, PullRequestStatus::Closed)
                .await?;
        }
    }

    Ok(())
}

#[cfg(not(test))]
fn now() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
thread_local! {
    static MOCK_TIME: std::cell::RefCell<Option<DateTime<Utc>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn now() -> DateTime<Utc> {
    MOCK_TIME.with(|time| time.borrow_mut().unwrap_or_else(Utc::now))
}

fn elapsed_time(date: DateTime<Utc>) -> Duration {
    let time: DateTime<Utc> = now();
    (time - date).to_std().unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use crate::bors::PullRequestStatus;
    use crate::bors::handlers::refresh::MOCK_TIME;
    use crate::bors::handlers::trybuild::TRY_BUILD_CHECK_RUN_NAME;
    use crate::database::{MergeableState, OctocrabMergeableState};
    use crate::tests::{BorsBuilder, BorsTester, GitHubState, default_repo_name, run_test};
    use chrono::Utc;
    use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};
    use std::future::Future;
    use std::time::Duration;
    use tokio::runtime::RuntimeFlavor;

    #[sqlx::test]
    async fn refresh_no_builds(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.cancel_timed_out_builds().await;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_pr_state(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.refresh_prs().await;
            Ok(())
        })
        .await;
    }

    fn gh_state_with_long_timeout() -> GitHubState {
        GitHubState::default().with_default_config(
            r#"
timeout = 3600
"#,
        )
    }

    #[sqlx::test]
    async fn refresh_do_nothing_before_timeout(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(gh_state_with_long_timeout())
            .run_test(async |tester: &mut BorsTester| {
                tester.post_comment("@bors try").await?;
                tester.expect_comments((), 1).await;
                with_mocked_time(Duration::from_secs(10), async {
                    tester.cancel_timed_out_builds().await;
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
            .run_test(async |tester: &mut BorsTester| {
                tester.post_comment("@bors try").await?;
                tester.expect_comments((), 1).await;
                with_mocked_time(Duration::from_secs(4000), async {
                    assert_eq!(
                        tester
                            .db()
                            .get_pending_builds(&default_repo_name())
                            .await
                            .unwrap()
                            .len(),
                        1
                    );
                    tester.cancel_timed_out_builds().await;
                })
                .await;
                insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @":boom: Test timed out after `3600`s");
                assert_eq!(
                    tester
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
    async fn refresh_cancel_build_updates_check_run(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(gh_state_with_long_timeout())
            .run_test(async |tester: &mut BorsTester| {
                tester.post_comment("@bors try").await?;
                tester.expect_comments((), 1).await;

                with_mocked_time(Duration::from_secs(4000), async {
                    tester.cancel_timed_out_builds().await;
                })
                .await;
                tester.expect_comments((), 1).await;

                tester
                    .expect_check_run(
                        &tester.get_pr_copy(()).await.get_gh_pr().head_sha,
                        TRY_BUILD_CHECK_RUN_NAME,
                        "Bors try build",
                        CheckRunStatus::Completed,
                        Some(CheckRunConclusion::TimedOut),
                    )
                    .await;

                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn refresh_cancel_workflow_after_timeout(pool: sqlx::PgPool) {
        let gh = BorsBuilder::new(pool)
            .github(gh_state_with_long_timeout())
            .run_test(async |tester: &mut BorsTester| {
                tester.post_comment("@bors try").await?;
                tester.expect_comments((), 1).await;
                tester.workflow_start(tester.try_branch().await).await?;

                with_mocked_time(Duration::from_secs(4000), async {
                    tester.cancel_timed_out_builds().await;
                })
                .await;
                tester.expect_comments((), 1).await;
                Ok(())
            })
            .await;
        gh.check_cancelled_workflows(default_repo_name(), &[1]);
    }

    #[sqlx::test]
    async fn refresh_enqueues_unknown_mergeable_prs(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .edit_pr((), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Unknown
                })
                .await?;
            tester
                .wait_for_pr((), |pr| pr.mergeable_state == MergeableState::Unknown)
                .await?;
            tester
                .modify_pr_state((), |pr| pr.mergeable_state = OctocrabMergeableState::Dirty)
                .await;
            tester.update_mergeability_status().await;
            tester
                .wait_for_pr((), |pr| pr.mergeable_state == MergeableState::HasConflicts)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_new_pr(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester
                .with_blocked_webhooks(async |tester: &mut BorsTester| {
                    tester.open_pr(default_repo_name(), |_| {}).await
                })
                .await?;
            tester.refresh_prs().await;
            tester
                .get_pr_copy(pr.number)
                .await
                .expect_status(PullRequestStatus::Open);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_pr_with_status_closed(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester.wait_for_pr(pr.number, |_| true).await?;
            tester
                .with_blocked_webhooks(async |tester: &mut BorsTester| {
                    tester.set_pr_status_closed(pr.number).await
                })
                .await?;
            tester.refresh_prs().await;
            tester
                .get_pr_copy(pr.number)
                .await
                .expect_status(PullRequestStatus::Closed);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_pr_with_status_draft(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester
                .with_blocked_webhooks(async |tester: &mut BorsTester| {
                    tester.set_pr_status_draft(pr.number).await
                })
                .await?;

            tester.refresh_prs().await;
            tester
                .get_pr_copy(pr.number)
                .await
                .expect_status(PullRequestStatus::Draft);
            Ok(())
        })
        .await;
    }

    async fn with_mocked_time<Fut: Future<Output = ()>>(in_future: Duration, future: Fut) {
        // It is important to use this function only with a single threaded runtime,
        // otherwise the `MOCK_TIME` variable might get mixed up between different threads.
        assert_eq!(
            tokio::runtime::Handle::current().runtime_flavor(),
            RuntimeFlavor::CurrentThread
        );
        MOCK_TIME.with(|time| {
            *time.borrow_mut() = Some(Utc::now() + chrono::Duration::from_std(in_future).unwrap());
        });
        future.await;
        MOCK_TIME.with(|time| {
            *time.borrow_mut() = None;
        });
    }
}
