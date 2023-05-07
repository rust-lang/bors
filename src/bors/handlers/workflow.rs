use crate::bors::event::{CheckSuiteCompleted, WorkflowCompleted, WorkflowStarted};
use crate::bors::handlers::is_bors_observed_branch;
use crate::bors::{self, RepositoryClient, RepositoryState};
use crate::database::{BuildStatus, DbClient, WorkflowStatus};

pub(super) async fn handle_workflow_started(
    db: &mut dyn DbClient,
    payload: WorkflowStarted,
) -> anyhow::Result<()> {
    if payload.run_id.is_none() {
        log::warn!("Ignoring workflow from external CI");
        return Ok(());
    }

    if !is_bors_observed_branch(&payload.branch) {
        return Ok(());
    }

    let Some(build) = db.find_build(&payload.repository, payload.branch.clone(), payload.commit_sha.clone()).await? else {
        log::warn!("Build for workflow {}/{}/{} not found", payload.repository, payload.branch, payload.commit_sha);
        return Ok(());
    };

    log::info!(
        "Storing workflow started into DB (name={}, url={}, run_id={:?})",
        payload.name,
        payload.url,
        payload.run_id
    );
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

pub(super) async fn handle_workflow_completed<Client: RepositoryClient>(
    repo: &mut RepositoryState<Client>,
    db: &mut dyn DbClient,
    payload: WorkflowCompleted,
) -> anyhow::Result<()> {
    log::info!(
        "Updating status of workflow with run ID {} to {:?}",
        payload.run_id,
        payload.status
    );
    db.update_workflow_status(payload.run_id, payload.status)
        .await?;

    // Try to complete the build
    let event = CheckSuiteCompleted {
        repository: payload.repository,
        branch: payload.branch,
        commit_sha: payload.commit_sha,
    };
    try_complete_build(repo, db, event).await
}

pub(super) async fn handle_check_suite_completed<Client: RepositoryClient>(
    repo: &mut RepositoryState<Client>,
    db: &mut dyn DbClient,
    payload: CheckSuiteCompleted,
) -> anyhow::Result<()> {
    try_complete_build(repo, db, payload).await
}

async fn try_complete_build<Client: RepositoryClient>(
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
            payload.branch.clone(),
            payload.commit_sha.clone(),
        )
        .await? else {
        log::warn!("Received check suite finished for an unknown build: {}/{}", payload.repository, payload.commit_sha);
        return Ok(());
    };

    // If the build has already been marked with a conclusion, ignore this event
    if build.status != BuildStatus::Pending {
        log::warn!(
            "Received check suite completed event for a build that already status {:?}",
            build.status
        );
        return Ok(());
    }

    let Some(pr) = db.find_pr_by_build(&build).await? else {
        log::warn!("Cannot find PR for build {}", build.commit_sha);
        return Ok(());
    };

    let checks = repo
        .client
        .get_check_suites_for_commit(&payload.branch, &payload.commit_sha)
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

    let mut workflows = db.get_workflows_for_build(&build).await?;
    workflows.sort_by(|a, b| a.name.cmp(&b.name));

    // If this happens, there is a race condition in GH webhooks and we haven't received a workflow
    // finished/failed event for some workflow yet. In this case, wait for that event before
    // posting the PR comment.
    if workflows
        .iter()
        .any(|w| w.status == WorkflowStatus::Pending)
    {
        return Ok(());
    }

    let workflow_list = workflows
        .into_iter()
        .map(|w| {
            format!(
                "- [{}]({}) {}",
                w.name,
                w.url,
                if w.status == WorkflowStatus::Success {
                    ":white_check_mark:"
                } else {
                    ":x:"
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let message = if !has_failure {
        let sha = &payload.commit_sha;
        format!(
            r#":sunny: Try build successful
{workflow_list}
Build commit: {sha} (`{sha}`)"#
        )
    } else {
        format!(
            r#":broken_heart: Test failed
{workflow_list}"#
        )
    };
    repo.client.post_comment(pr.number, &message).await?;

    let status = if has_failure {
        BuildStatus::Failure
    } else {
        BuildStatus::Success
    };
    db.update_build_status(&build, status).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{assert_eq, vec};

    use sea_orm::EntityTrait;

    use entity::workflow;

    use crate::bors::handlers::trybuild::TRY_BRANCH_NAME;
    use crate::database::WorkflowStatus;
    use crate::github::CommitSha;
    use crate::tests::event::{
        default_pr_number, suite_failure, suite_pending, suite_success, CheckSuiteCompletedBuilder,
        WorkflowCompletedBuilder, WorkflowStartedBuilder,
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
    async fn test_try_success() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().merge_branches_fn = Box::new(|| Ok(CommitSha("sha1".to_string())));
        state.client().set_checks("sha1", vec![suite_success()]);

        state.comment("@bors try").await;
        state
            .perform_workflow_events(1, TRY_BRANCH_NAME, "sha1", WorkflowStatus::Success)
            .await;

        insta::assert_snapshot!(
            state.client().get_last_comment(default_pr_number()),
            @r###"
        :sunny: Try build successful
        - [workflow-1](https://workflow-1.com) :white_check_mark:
        Build commit: sha1 (`sha1`)
        "###
        );
    }

    #[tokio::test]
    async fn test_try_failure() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().merge_branches_fn = Box::new(|| Ok(CommitSha("sha1".to_string())));
        state.client().set_checks("sha1", vec![suite_failure()]);

        state.comment("@bors try").await;
        state
            .perform_workflow_events(1, TRY_BRANCH_NAME, "sha1", WorkflowStatus::Failure)
            .await;

        insta::assert_snapshot!(
            state.client().get_last_comment(default_pr_number()),
            @r###"
        :broken_heart: Test failed
        - [workflow-1](https://workflow-1.com) :x:
        "###
        );
    }

    #[tokio::test]
    async fn test_try_success_multiple_suites() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().merge_branches_fn = Box::new(|| Ok(CommitSha("sha1".to_string())));
        state
            .client()
            .set_checks("sha1", vec![suite_success(), suite_pending()]);

        state.comment("@bors try").await;
        state
            .perform_workflow_events(1, TRY_BRANCH_NAME, "sha1", WorkflowStatus::Success)
            .await;
        state.client().check_comment_count(default_pr_number(), 1);

        state
            .client()
            .set_checks("sha1", vec![suite_success(), suite_success()]);
        state
            .perform_workflow_events(2, TRY_BRANCH_NAME, "sha1", WorkflowStatus::Success)
            .await;
        insta::assert_snapshot!(state.client().get_last_comment(default_pr_number()), @r###"
        :sunny: Try build successful
        - [workflow-1](https://workflow-1.com) :white_check_mark:
        - [workflow-2](https://workflow-2.com) :white_check_mark:
        Build commit: sha1 (`sha1`)
        "###);
    }

    #[tokio::test]
    async fn test_try_failure_multiple_suites() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().merge_branches_fn = Box::new(|| Ok(CommitSha("sha1".to_string())));
        state
            .client()
            .set_checks("sha1", vec![suite_success(), suite_pending()]);

        state.comment("@bors try").await;
        state
            .perform_workflow_events(1, TRY_BRANCH_NAME, "sha1", WorkflowStatus::Success)
            .await;
        state.client().check_comment_count(default_pr_number(), 1);

        state
            .client()
            .set_checks("sha1", vec![suite_success(), suite_failure()]);
        state
            .perform_workflow_events(2, TRY_BRANCH_NAME, "sha1", WorkflowStatus::Failure)
            .await;
        insta::assert_snapshot!(state.client().get_last_comment(default_pr_number()), @r###"
        :broken_heart: Test failed
        - [workflow-1](https://workflow-1.com) :white_check_mark:
        - [workflow-2](https://workflow-2.com) :x:
        "###);
    }

    #[tokio::test]
    async fn test_try_suite_completed_received_before_workflow_completed() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().merge_branches_fn = Box::new(|| Ok(CommitSha("sha1".to_string())));
        state.client().set_checks("sha1", vec![suite_success()]);

        state.comment("@bors try").await;
        state
            .workflow_started(
                WorkflowStartedBuilder::default()
                    .branch(TRY_BRANCH_NAME.to_string())
                    .commit_sha("sha1".to_string()),
            )
            .await;

        // Check suite completed received before the workflow has finished.
        // We should wait until workflow finished is received before posting the comment.
        state
            .check_suite_completed(
                CheckSuiteCompletedBuilder::default()
                    .branch(TRY_BRANCH_NAME.to_string())
                    .commit_sha("sha1".to_string()),
            )
            .await;
        state.client().check_comment_count(default_pr_number(), 1);

        state
            .workflow_completed(
                WorkflowCompletedBuilder::default()
                    .branch(TRY_BRANCH_NAME.to_string())
                    .commit_sha("sha1".to_string())
                    .status(WorkflowStatus::Success),
            )
            .await;

        insta::assert_snapshot!(state.client().get_last_comment(default_pr_number()), @r###"
        :sunny: Try build successful
        - [workflow-name](https://workflow.com) :white_check_mark:
        Build commit: sha1 (`sha1`)
        "###);
    }

    #[tokio::test]
    async fn test_try_check_suite_finished_twice() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().merge_branches_fn = Box::new(|| Ok(CommitSha("sha1".to_string())));
        state
            .client()
            .set_checks("sha1", vec![suite_success(), suite_success()]);

        let event = || {
            CheckSuiteCompletedBuilder::default()
                .branch(TRY_BRANCH_NAME.to_string())
                .commit_sha("sha1".to_string())
        };

        state.comment("@bors try").await;
        state.check_suite_completed(event()).await;
        state.check_suite_completed(event()).await;
        state.client().check_comment_count(default_pr_number(), 2);
    }
}
