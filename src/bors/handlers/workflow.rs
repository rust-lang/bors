use crate::bors::event::{CheckSuiteCompleted, WorkflowStarted};
use crate::bors::handlers::is_bors_observed_branch;
use crate::bors::{self, RepositoryClient, RepositoryState};
use crate::database::{DbClient, WorkflowStatus};

pub(super) async fn handle_workflow_started(
    db: &mut dyn DbClient,
    payload: WorkflowStarted,
) -> anyhow::Result<()> {
    if !is_bors_observed_branch(&payload.branch) {
        return Ok(());
    }

    let Some(build) = db.find_build(&payload.repository, payload.branch.clone(), payload.commit_sha.clone()).await? else {
        log::warn!("Build for workflow {}/{}/{} not found", payload.repository, payload.branch, payload.commit_sha);
        return Ok(());
    };

    db.create_workflow(
        &build,
        payload.name,
        payload.url,
        payload.run_id,
        WorkflowStatus::Pending,
    )
    .await?;

    Ok(())
}

pub(super) async fn handle_check_suite_completed<Client: RepositoryClient>(
    repo: &mut RepositoryState<Client>,
    db: &mut dyn DbClient,
    payload: CheckSuiteCompleted,
) -> anyhow::Result<()> {
    if !is_bors_observed_branch(&payload.branch) {
        return Ok(());
    }

    let Some(build) = db
        .find_build(
            &payload.repository,
            payload.branch,
            payload.commit_sha.clone(),
        )
        .await? else {
        log::warn!("Received check suite finished for an unknown build: {}/{}", payload.repository, payload.commit_sha);
        return Ok(());
    };
    let Some(pr) = db.find_pr_by_build(&build).await? else {
        log::warn!("Cannot find PR for build {}", build.commit_sha);
        return Ok(());
    };

    let checks = repo
        .client
        .get_check_suites_for_commit(&payload.commit_sha)
        .await?;

    // Some checks are still running, let's wait for the next event
    if checks
        .iter()
        .any(|check| matches!(check.status, bors::CheckSuiteStatus::Pending))
    {
        return Ok(());
    }

    let has_failure = checks
        .iter()
        .any(|check| matches!(check.status, bors::CheckSuiteStatus::Failure));

    let message = if !has_failure {
        let sha = &payload.commit_sha;
        format!(
            r#":sunny: Try build successful
Build commit: {sha} (`{sha}`)"#,
        )
    } else {
        ":broken_heart: Test failed".to_string()
    };
    repo.client.post_comment(pr.number, &message).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use entity::workflow;
    use sea_orm::EntityTrait;

    use crate::bors::handlers::trybuild::TRY_BRANCH_NAME;
    use crate::github::CommitSha;
    use crate::tests::event::{
        default_pr_number, suite_failure, suite_pending, suite_success, CheckSuiteCompletedBuilder,
        WorkflowStartedBuilder,
    };
    use crate::tests::state::ClientBuilder;

    #[tokio::test]
    async fn test_unknown_build() {
        let mut state = ClientBuilder::default().create_state().await;

        state
            .workflow_started(
                WorkflowStartedBuilder::default()
                    .branch("unknown".to_string())
                    .commit_sha("unknown-sha-".to_string()),
            )
            .await;
        assert!(workflow::Entity::find()
            .one(state.db.connection())
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_try_workflow_started() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().merge_branches_fn = Box::new(|| Ok(CommitSha("sha1".to_string())));
        state.comment("@bors try").await;

        state
            .workflow_started(
                WorkflowStartedBuilder::default()
                    .branch(TRY_BRANCH_NAME.to_string())
                    .commit_sha("sha1".to_string())
                    .run_id(Some(42)),
            )
            .await;
        let suite = workflow::Entity::find()
            .one(state.db.connection())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(suite.status, "pending");
    }

    #[tokio::test]
    async fn test_try_workflow_start_twice() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().merge_branches_fn = Box::new(|| Ok(CommitSha("sha1".to_string())));
        state.comment("@bors try").await;

        let event = || {
            WorkflowStartedBuilder::default()
                .branch(TRY_BRANCH_NAME.to_string())
                .commit_sha("sha1".to_string())
                .run_id(Some(42))
        };
        state.workflow_started(event()).await;
        state.workflow_started(event()).await;
        assert_eq!(
            workflow::Entity::find()
                .all(state.db.connection())
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn test_try_check_suite_finished_missing_build() {
        let mut state = ClientBuilder::default().create_state().await;
        state
            .check_suite_completed(
                CheckSuiteCompletedBuilder::default()
                    .branch("<branch>".to_string())
                    .commit_sha("<unknown-sha>".to_string()),
            )
            .await;
    }

    #[tokio::test]
    async fn test_try_check_suite_finished_success() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().merge_branches_fn = Box::new(|| Ok(CommitSha("sha1".to_string())));
        state
            .client()
            .check_suites
            .insert("sha1".to_string(), vec![suite_success()]);

        state.comment("@bors try").await;
        state
            .check_suite_completed(
                CheckSuiteCompletedBuilder::default()
                    .branch(TRY_BRANCH_NAME.to_string())
                    .commit_sha("sha1".to_string()),
            )
            .await;
        assert_eq!(
            state.client().get_last_comment(default_pr_number()),
            ":sunny: Try build successful\nBuild commit: sha1 (`sha1`)"
        );
    }

    #[tokio::test]
    async fn test_try_check_suite_finished_failure() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().merge_branches_fn = Box::new(|| Ok(CommitSha("sha1".to_string())));
        state
            .client()
            .check_suites
            .insert("sha1".to_string(), vec![suite_failure()]);

        state.comment("@bors try").await;
        state
            .check_suite_completed(
                CheckSuiteCompletedBuilder::default()
                    .branch(TRY_BRANCH_NAME.to_string())
                    .commit_sha("sha1".to_string()),
            )
            .await;
        assert_eq!(
            state.client().get_last_comment(default_pr_number()),
            ":broken_heart: Test failed"
        );
    }

    #[tokio::test]
    async fn test_try_check_suite_finished_multiple_checks() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().merge_branches_fn = Box::new(|| Ok(CommitSha("sha1".to_string())));
        state
            .client()
            .check_suites
            .insert("sha1".to_string(), vec![suite_success(), suite_pending()]);

        let event = || {
            CheckSuiteCompletedBuilder::default()
                .branch(TRY_BRANCH_NAME.to_string())
                .commit_sha("sha1".to_string())
        };

        state.comment("@bors try").await;
        state.check_suite_completed(event()).await;
        state.client().check_comments(
            default_pr_number(),
            &[":hourglass: Trying commit pr-sha with merge sha1…"],
        );

        state
            .client()
            .check_suites
            .insert("sha1".to_string(), vec![suite_success(), suite_success()]);
        state.check_suite_completed(event()).await;
        assert_eq!(
            state.client().get_last_comment(default_pr_number()),
            ":sunny: Try build successful\nBuild commit: sha1 (`sha1`)"
        );
    }
}
