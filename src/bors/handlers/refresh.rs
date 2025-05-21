use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

use crate::bors::Comment;
use crate::bors::PullRequestStatus;
use crate::bors::RepositoryState;
use crate::bors::handlers::trybuild::cancel_build_workflows;
use crate::bors::mergeable_queue::MergeableQueueSender;
use crate::database::BuildStatus;
use crate::{PgDbClient, TeamApiClient};

/// Cancel CI builds that have been running for too long
pub async fn cancel_timed_out_builds(
    repo: Arc<RepositoryState>,
    db: &PgDbClient,
) -> anyhow::Result<()> {
    let running_builds = db.get_running_builds(repo.repository()).await?;
    tracing::info!("Found {} running build(s)", running_builds.len());

    let timeout = repo.config.load().timeout;
    for build in running_builds {
        if elapsed_time(build.created_at) >= timeout {
            tracing::info!("Cancelling build {}", build.commit_sha);

            db.update_build_status(&build, BuildStatus::Cancelled)
                .await?;
            if let Some(pr) = db.find_pr_by_build(&build).await? {
                if let Err(error) = cancel_build_workflows(&repo.client, db, &build).await {
                    tracing::error!(
                        "Could not cancel workflows for SHA {}: {error:?}",
                        build.commit_sha
                    );
                }

                if let Err(error) = repo
                    .client
                    .post_comment(pr.number, Comment::new(":boom: Test timed out".to_string()))
                    .await
                {
                    tracing::error!("Could not send comment to PR {}: {error:?}", pr.number);
                }
            } else {
                tracing::warn!("No PR found for build {}", build.commit_sha);
            }
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
pub async fn reload_unknown_mergeable_prs(
    repo: Arc<RepositoryState>,
    db: &PgDbClient,
    mergeable_queue: MergeableQueueSender,
) -> anyhow::Result<()> {
    let prs = db
        .get_prs_with_unknown_mergeable_state(repo.repository())
        .await?;

    tracing::info!(
        "Refreshing {} PR(s) with unknown mergeable state",
        prs.len()
    );

    for pr in prs {
        mergeable_queue.enqueue(repo.repository().clone(), pr.number);
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
    let repo = repo.as_ref();
    let db = db.as_ref();
    let repo_name = repo.repository();
    // load open/draft prs from github
    let nonclosed_gh_prs = repo.client.fetch_nonclosed_pull_requests().await?;
    // load open/draft prs from db
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
            db.upsert_pull_request(
                repo_name,
                gh_pr.number,
                &gh_pr.base.name,
                gh_pr.mergeable_state.clone().into(),
                &gh_pr.status,
            )
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
    use crate::bors::handlers::WAIT_FOR_WORKFLOW_STARTED;
    use crate::bors::handlers::refresh::MOCK_TIME;
    use crate::database::{MergeableState, OctocrabMergeableState};
    use crate::tests::mocks::{
        BorsBuilder, GitHubState, WorkflowEvent, default_pr_number, default_repo_name, run_test,
    };
    use chrono::Utc;
    use std::future::Future;
    use std::time::Duration;
    use tokio::runtime::RuntimeFlavor;

    #[sqlx::test]
    async fn refresh_no_builds(pool: sqlx::PgPool) {
        run_test(pool, |tester| async move {
            tester.cancel_timed_out_builds().await;
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_pr_state(pool: sqlx::PgPool) {
        run_test(pool, |tester| async move {
            tester.refresh_prs().await;
            Ok(tester)
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
            .run_test(|mut tester| async move {
                tester.post_comment("@bors try").await?;
                tester.expect_comments(1).await;
                with_mocked_time(Duration::from_secs(10), async {
                    tester.cancel_timed_out_builds().await;
                })
                .await;
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn refresh_cancel_build_after_timeout(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(gh_state_with_long_timeout())
            .run_test(|mut tester| async move {
                tester.post_comment("@bors try").await?;
                tester.expect_comments(1).await;
                with_mocked_time(Duration::from_secs(4000), async {
                    assert_eq!(
                        tester
                            .db()
                            .get_running_builds(&default_repo_name())
                            .await
                            .unwrap()
                            .len(),
                        1
                    );
                    tester.cancel_timed_out_builds().await;
                })
                .await;
                insta::assert_snapshot!(tester.get_comment().await?, @":boom: Test timed out");
                assert_eq!(
                    tester
                        .db()
                        .get_running_builds(&default_repo_name())
                        .await
                        .unwrap()
                        .len(),
                    0
                );
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn refresh_cancel_workflow_after_timeout(pool: sqlx::PgPool) {
        let gh = BorsBuilder::new(pool)
            .github(gh_state_with_long_timeout())
            .run_test(|mut tester| async move {
                tester.post_comment("@bors try").await?;
                tester.expect_comments(1).await;
                tester
                    .workflow_event(WorkflowEvent::started(tester.try_branch()))
                    .await?;
                WAIT_FOR_WORKFLOW_STARTED.sync().await;

                with_mocked_time(Duration::from_secs(4000), async {
                    tester.cancel_timed_out_builds().await;
                })
                .await;
                tester.expect_comments(1).await;
                Ok(tester)
            })
            .await;
        gh.check_cancelled_workflows(default_repo_name(), &[1]);
    }

    #[sqlx::test]
    async fn refresh_enqueues_unknown_mergeable_prs(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester
                .edit_pr(default_repo_name(), default_pr_number(), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Unknown
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
            tester.update_mergeability_status().await;
            tester
                .wait_for_default_pr(|pr| pr.mergeable_state == MergeableState::HasConflicts)
                .await?;
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_new_pr(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async move {
            let pr = tester
                .with_blocked_webhooks(async |tester| {
                    tester.open_pr(default_repo_name(), false).await
                })
                .await?;
            tester.refresh_prs().await;
            assert_eq!(
                tester
                    .db()
                    .get_pull_request(&default_repo_name(), pr.number)
                    .await?
                    .unwrap()
                    .pr_status,
                PullRequestStatus::Open
            );
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_pr_with_status_closed(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async move {
            let pr = tester.open_pr(default_repo_name(), false).await?;
            tester
                .wait_for_pr(default_repo_name(), pr.number.0, |_| true)
                .await?;
            tester
                .with_blocked_webhooks(async |tester| {
                    tester.close_pr(default_repo_name(), pr.number.0).await
                })
                .await?;
            tester.refresh_prs().await;
            assert_eq!(
                tester
                    .db()
                    .get_pull_request(&default_repo_name(), pr.number)
                    .await?
                    .unwrap()
                    .pr_status,
                PullRequestStatus::Closed
            );
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_pr_with_status_draft(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async move {
            let pr = tester.open_pr(default_repo_name(), false).await?;
            tester
                .with_blocked_webhooks(async |tester| {
                    tester
                        .convert_to_draft(default_repo_name(), pr.number.0)
                        .await
                })
                .await?;

            tester.refresh_prs().await;
            assert_eq!(
                tester
                    .db()
                    .get_pull_request(&default_repo_name(), pr.number)
                    .await?
                    .unwrap()
                    .pr_status,
                PullRequestStatus::Draft
            );
            Ok(tester)
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
