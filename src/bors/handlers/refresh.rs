use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::bors::handlers::trybuild::cancel_build_workflows;
use crate::bors::{RepositoryClient, RepositoryState};
use crate::database::{BuildStatus, DbClient};

pub async fn refresh_repository<Client: RepositoryClient>(
    repo: &mut RepositoryState<Client>,
    db: &dyn DbClient,
) -> anyhow::Result<()> {
    let res = cancel_timed_out_builds(repo, db).await;
    reload_permission(repo).await;
    reload_config(repo).await?;

    res
}

async fn cancel_timed_out_builds<Client: RepositoryClient>(
    repo: &mut RepositoryState<Client>,
    db: &dyn DbClient,
) -> anyhow::Result<()> {
    let running_builds = db.get_running_builds(&repo.repository).await?;
    tracing::info!("Found {} running build(s)", running_builds.len());

    for build in running_builds {
        let timeout = repo.config.timeout;
        if elapsed_time(build.created_at) >= timeout {
            tracing::info!("Cancelling build {}", build.commit_sha);

            db.update_build_status(&build, BuildStatus::Cancelled)
                .await?;
            if let Some(pr) = db.find_pr_by_build(&build).await? {
                if let Err(error) = cancel_build_workflows(repo, db, &build).await {
                    tracing::error!(
                        "Could not cancel workflows for SHA {}: {error:?}",
                        build.commit_sha
                    );
                }

                if let Err(error) = repo
                    .client
                    .post_comment(pr.number, ":boom: Test timed out")
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

async fn reload_permission<Client: RepositoryClient>(repo: &mut RepositoryState<Client>) {
    repo.permissions_resolver.reload().await
}

async fn reload_config<Client: RepositoryClient>(
    repo: &mut RepositoryState<Client>,
) -> anyhow::Result<()> {
    let config = repo.client.load_config().await?;
    repo.config = config;
    Ok(())
}

#[cfg(not(test))]
fn now() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
thread_local! {
    static MOCK_TIME: std::cell::RefCell<Option<DateTime<Utc>>> = std::cell::RefCell::new(None);
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
    use std::future::Future;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use chrono::Utc;
    use tokio::runtime::RuntimeFlavor;

    use crate::bors::handlers::refresh::MOCK_TIME;
    use crate::bors::handlers::trybuild::TRY_BRANCH_NAME;
    use crate::database::DbClient;
    use crate::tests::event::{default_pr_number, WorkflowStartedBuilder};
    use crate::tests::permissions::MockPermissions;
    use crate::tests::state::{default_repo_name, ClientBuilder, RepoConfigBuilder};

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_no_builds() {
        let mut state = ClientBuilder::default().create_state().await;
        state.refresh().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_permission() {
        let permission_resolver = Arc::new(Mutex::new(MockPermissions::default()));
        let mut state = ClientBuilder::default()
            .permission_resolver(Box::new(Arc::clone(&permission_resolver)))
            .create_state()
            .await;
        state.refresh().await;
        assert_eq!(permission_resolver.lock().unwrap().num_reload_called, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_do_nothing_before_timeout() {
        let mut state = ClientBuilder::default()
            .config(RepoConfigBuilder::default().timeout(Duration::from_secs(3600)))
            .create_state()
            .await;
        state.comment("@bors try").await;
        with_mocked_time(Duration::from_secs(10), async move {
            state.refresh().await;
            state.client().check_comment_count(default_pr_number(), 1);
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_cancel_build_after_timeout() {
        let mut state = ClientBuilder::default()
            .config(RepoConfigBuilder::default().timeout(Duration::from_secs(3600)))
            .create_state()
            .await;
        state.comment("@bors try").await;
        with_mocked_time(Duration::from_secs(4000), async move {
            assert_eq!(state.db.get_running_builds(&default_repo_name()).await.unwrap().len(), 1);
            state.refresh().await;
            insta::assert_snapshot!(state.client().get_last_comment(default_pr_number()), @":boom: Test timed out");
            assert_eq!(state.db.get_running_builds(&default_repo_name()).await.unwrap().len(), 0);
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_cancel_workflow_after_timeout() {
        let mut state = ClientBuilder::default()
            .config(RepoConfigBuilder::default().timeout(Duration::from_secs(3600)))
            .create_state()
            .await;
        state.comment("@bors try").await;
        state
            .workflow_started(
                WorkflowStartedBuilder::default()
                    .branch(TRY_BRANCH_NAME.to_string())
                    .run_id(123),
            )
            .await;

        with_mocked_time(Duration::from_secs(4000), async move {
            state.refresh().await;
            state.client().check_cancelled_workflows(&[123]);
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
