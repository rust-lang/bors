use crate::bors::event::WorkflowStarted;
use crate::database::{CheckSuiteStatus, DbClient};

pub(super) async fn handle_workflow_started(
    db: &mut dyn DbClient,
    payload: WorkflowStarted,
) -> anyhow::Result<()> {
    let Some(build) = db.find_build(&payload.repository, payload.branch.clone(), payload.commit_sha.clone()).await? else {
        log::warn!("Build for workflow {}/{}/{} not found", payload.repository, payload.branch, payload.commit_sha);
        return Ok(());
    };

    db.create_check_suite(
        &build,
        payload.check_suite_id,
        payload.workflow_run_id,
        CheckSuiteStatus::Started,
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use sea_orm::EntityTrait;

    use entity::check_suite;

    use crate::bors::handlers::trybuild::TRY_BRANCH_NAME;
    use crate::github::CommitSha;
    use crate::tests::event::WorkflowStartedBuilder;
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
        assert!(check_suite::Entity::find()
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
                    .run_id(42)
                    .check_suite_id(102),
            )
            .await;
        let suite = check_suite::Entity::find()
            .one(state.db.connection())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(suite.workflow_run_id, Some(42));
        assert_eq!(suite.check_suite_id, 102);
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
                .run_id(42)
                .check_suite_id(102)
        };
        state.workflow_started(event()).await;
        state.workflow_started(event()).await;
        assert_eq!(
            check_suite::Entity::find()
                .all(state.db.connection())
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
