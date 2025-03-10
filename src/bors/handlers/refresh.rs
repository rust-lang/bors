use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};

use crate::bors::handlers::trybuild::cancel_build_workflows;
use crate::bors::Comment;
use crate::bors::RepositoryState;
use crate::database::BuildStatus;
use crate::{PgDbClient, TeamApiClient};

pub async fn refresh_repository(
    repo: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    team_api_client: &TeamApiClient,
) -> anyhow::Result<()> {
    let repo = repo.as_ref();
    if let (Ok(_), _, Ok(_)) = tokio::join!(
        cancel_timed_out_builds(repo, db.as_ref()),
        reload_permission(repo, team_api_client),
        reload_config(repo)
    ) {
        Ok(())
    } else {
        tracing::error!("Failed to refresh repository");
        anyhow::bail!("Failed to refresh repository")
    }
}

async fn cancel_timed_out_builds(repo: &RepositoryState, db: &PgDbClient) -> anyhow::Result<()> {
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

async fn reload_permission(
    repo: &RepositoryState,
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

async fn reload_config(repo: &RepositoryState) -> anyhow::Result<()> {
    let config = repo.client.load_config().await?;
    repo.config.store(Arc::new(config));
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
    use crate::bors::handlers::refresh::MOCK_TIME;
    use crate::bors::handlers::WAIT_FOR_WORKFLOW_STARTED;
    use crate::tests::mocks::{default_repo_name, run_test, BorsBuilder, WorkflowEvent, World};
    use chrono::Utc;
    use std::future::Future;
    use std::time::Duration;
    use tokio::runtime::RuntimeFlavor;

    #[sqlx::test]
    async fn refresh_no_builds(pool: sqlx::PgPool) {
        run_test(pool, |tester| async move {
            tester.refresh().await;
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn refresh_do_nothing_before_timeout(pool: sqlx::PgPool) {
        let world = World::default();
        world.default_repo().lock().set_config(
            r#"
timeout = 3600
"#,
        );
        BorsBuilder::new(pool)
            .world(world)
            .run_test(|mut tester| async move {
                tester.post_comment("@bors try").await?;
                tester.expect_comments(1).await;
                with_mocked_time(Duration::from_secs(10), async {
                    tester.refresh().await;
                })
                .await;
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn refresh_cancel_build_after_timeout(pool: sqlx::PgPool) {
        let world = World::default();
        world.default_repo().lock().set_config(
            r#"
timeout = 3600
"#,
        );
        BorsBuilder::new(pool)
            .world(world)
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
                    tester.refresh().await;
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
        let world = World::default();
        world.default_repo().lock().set_config(
            r#"
timeout = 3600
"#,
        );
        let world = BorsBuilder::new(pool)
            .world(world)
            .run_test(|mut tester| async move {
                tester.post_comment("@bors try").await?;
                tester.expect_comments(1).await;
                tester
                    .workflow_event(WorkflowEvent::started(tester.try_branch()))
                    .await?;
                WAIT_FOR_WORKFLOW_STARTED.sync().await;

                with_mocked_time(Duration::from_secs(4000), async {
                    tester.refresh().await;
                })
                .await;
                tester.expect_comments(1).await;
                Ok(tester)
            })
            .await;
        world.check_cancelled_workflows(default_repo_name(), &[1]);
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
