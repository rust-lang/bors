use crate::bors::handlers::labels::handle_label_trigger;
use anyhow::anyhow;

use crate::bors::RepositoryClient;
use crate::bors::RepositoryState;
use crate::database::{
    BuildModel, BuildStatus, DbClient, PullRequestModel, WorkflowStatus, WorkflowType,
};
use crate::github::{GithubUser, LabelTrigger, MergeError, PullRequest, PullRequestNumber};
use crate::permissions::PermissionType;

// This branch serves for preparing the final commit.
// It will be reset to master and merged with the branch that should be tested.
// Because this action (reset + merge) is not atomic, this branch should not run CI checks to avoid
// starting them twice.
const TRY_MERGE_BRANCH_NAME: &str = "automation/bors/try-merge";

// This branch should run CI checks.
pub(super) const TRY_BRANCH_NAME: &str = "automation/bors/try";

/// Performs a so-called try build - merges the PR branch into a special branch designed
/// for running CI checks.
pub(super) async fn command_try_build<Client: RepositoryClient>(
    repo: &mut RepositoryState<Client>,
    db: &mut dyn DbClient,
    pr: &PullRequest,
    author: &GithubUser,
) -> anyhow::Result<()> {
    if !check_try_permissions(repo, pr, author).await? {
        return Ok(());
    }

    let pr_model = db
        .get_or_create_pull_request(repo.client.repository(), pr.number)
        .await?;

    if let Some(ref build) = pr_model.try_build {
        if build.status == BuildStatus::Pending {
            tracing::warn!("Try build already in progress");
            repo.client
                .post_comment(
                    pr.number,
                    ":exclamation: A try build is currently in progress. You can cancel it using @bors try cancel.",
                )
                .await?;
            return Ok(());
        }
    }

    repo.client
        .set_branch_to_sha(TRY_MERGE_BRANCH_NAME, &pr.base.sha)
        .await
        .map_err(|error| anyhow!("Cannot set try merge branch to main branch: {error:?}"))?;
    match repo
        .client
        .merge_branches(
            TRY_MERGE_BRANCH_NAME,
            &pr.head.sha,
            &auto_merge_commit_message(pr, "<try>"),
        )
        .await
    {
        Ok(merge_sha) => {
            tracing::debug!("Merge successful, SHA: {merge_sha}");
            repo.client
                .set_branch_to_sha(TRY_BRANCH_NAME, &merge_sha)
                .await
                .map_err(|error| anyhow!("Cannot set try branch to main branch: {error:?}"))?;

            db.attach_try_build(pr_model, TRY_BRANCH_NAME.to_string(), merge_sha.clone())
                .await?;
            tracing::info!("Try build started");

            handle_label_trigger(repo, pr.number, LabelTrigger::TryBuildStarted).await?;

            repo.client
                .post_comment(
                    pr.number,
                    &format!(
                        ":hourglass: Trying commit {} with merge {merge_sha}…",
                        pr.head.sha
                    ),
                )
                .await?;
            Ok(())
        }
        Err(MergeError::Conflict) => {
            tracing::warn!("Merge conflict");
            repo.client
                .post_comment(pr.number, &merge_conflict_message(&pr.head.name))
                .await?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) async fn command_try_cancel<Client: RepositoryClient>(
    repo: &mut RepositoryState<Client>,
    db: &mut dyn DbClient,
    pr: &PullRequest,
    author: &GithubUser,
) -> anyhow::Result<()> {
    if !check_try_permissions(repo, pr, author).await? {
        return Ok(());
    }

    let pr_number: PullRequestNumber = pr.number;
    let pr = db
        .get_or_create_pull_request(repo.client.repository(), pr_number)
        .await?;

    let Some(build) = get_pending_build(pr) else {
        tracing::warn!("No build found");
        repo.client.post_comment(pr_number, ":exclamation: There is currently no try build in progress.").await?;
        return Ok(());
    };

    if let Err(error) = cancel_build_workflows(repo, db, &build).await {
        tracing::error!(
            "Could not cancel workflows for SHA {}: {error:?}",
            build.commit_sha
        );
    }

    db.update_build_status(&build, BuildStatus::Cancelled)
        .await?;

    tracing::info!("Try build cancelled");

    repo.client
        .post_comment(pr_number, "Try build cancelled.")
        .await?;

    Ok(())
}

pub async fn cancel_build_workflows<Client: RepositoryClient>(
    repo: &mut RepositoryState<Client>,
    db: &dyn DbClient,
    build: &BuildModel,
) -> anyhow::Result<()> {
    let pending_workflows = db
        .get_workflows_for_build(build)
        .await?
        .into_iter()
        .filter(|w| w.status == WorkflowStatus::Pending && w.workflow_type == WorkflowType::Github)
        .map(|w| w.run_id)
        .collect::<Vec<_>>();

    tracing::info!("Cancelling workflows {:?}", pending_workflows);
    repo.client.cancel_workflows(pending_workflows).await
}

fn get_pending_build(pr: PullRequestModel) -> Option<BuildModel> {
    pr.try_build
        .and_then(|b| (b.status == BuildStatus::Pending).then_some(b))
}

fn auto_merge_commit_message(pr: &PullRequest, reviewer: &str) -> String {
    let pr_number = pr.number;
    format!(
        r#"Auto merge of #{pr_number} - {pr_label}, r={reviewer}
{pr_title}

{pr_message}"#,
        pr_label = pr.head_label,
        pr_title = pr.title,
        pr_message = pr.message
    )
}

fn merge_conflict_message(branch: &str) -> String {
    format!(
        r#":lock: Merge conflict

This pull request and the master branch diverged in a way that cannot
 be automatically merged. Please rebase on top of the latest master
 branch, and let the reviewer approve again.

<details><summary>How do I rebase?</summary>

Assuming `self` is your fork and `upstream` is this repository,
 you can resolve the conflict following these steps:

1. `git checkout {branch}` *(switch to your branch)*
2. `git fetch upstream master` *(retrieve the latest master)*
3. `git rebase upstream/master -p` *(rebase on top of it)*
4. Follow the on-screen instruction to resolve conflicts (check `git status` if you got lost).
5. `git push self {branch} --force-with-lease` *(update this PR)*

You may also read
 [*Git Rebasing to Resolve Conflicts* by Drew Blessing](http://blessing.io/git/git-rebase/open-source/2015/08/23/git-rebasing-to-resolve-conflicts.html)
 for a short tutorial.

Please avoid the ["**Resolve conflicts**" button](https://help.github.com/articles/resolving-a-merge-conflict-on-github/) on GitHub.
 It uses `git merge` instead of `git rebase` which makes the PR commit history more difficult to read.

Sometimes step 4 will complete without asking for resolution. This is usually due to difference between how `Cargo.lock` conflict is
handled during merge and rebase. This is normal, and you should still perform step 5 to update this PR.

</details>  
"#
    )
}

async fn check_try_permissions<Client: RepositoryClient>(
    repo: &mut RepositoryState<Client>,
    pr: &PullRequest,
    author: &GithubUser,
) -> anyhow::Result<bool> {
    let result = if !repo
        .permissions_resolver
        .has_permission(&author.username, PermissionType::Try)
        .await
    {
        tracing::info!("Permission denied");
        repo.client
            .post_comment(
                pr.number,
                &format!(
                    "@{}: :key: Insufficient privileges: not in try users",
                    author.username
                ),
            )
            .await?;
        false
    } else {
        true
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use sea_orm::EntityTrait;

    use entity::workflow;

    use crate::bors::handlers::trybuild::{TRY_BRANCH_NAME, TRY_MERGE_BRANCH_NAME};
    use crate::database::{BuildStatus, DbClient, WorkflowStatus, WorkflowType};
    use crate::github::{CommitSha, LabelTrigger, MergeError};
    use crate::tests::event::{
        default_pr_number, suite_failure, suite_pending, suite_success, WorkflowStartedBuilder,
    };
    use crate::tests::github::{BranchBuilder, PRBuilder};
    use crate::tests::permissions::NoPermissions;
    use crate::tests::state::{
        default_merge_sha, default_repo_name, ClientBuilder, RepoConfigBuilder,
    };

    #[tokio::test]
    async fn test_try_no_permission() {
        let mut state = ClientBuilder::default()
            .permission_resolver(Box::new(NoPermissions))
            .create_state()
            .await;
        state.comment("@bors try").await;
        state.client().check_comments(
            default_pr_number(),
            &["@<user>: :key: Insufficient privileges: not in try users"],
        );
    }

    #[tokio::test]
    async fn test_try_merge_comment() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().get_pr_fn = Box::new(|pr| {
            Ok(PRBuilder::default()
                .number(pr.0)
                .head(BranchBuilder::default().sha("head1".to_string()).create())
                .create())
        });

        state.comment("@bors try").await;

        state.client().check_comments(
            default_pr_number(),
            &[":hourglass: Trying commit head1 with merge sha-merged…"],
        );
    }

    #[tokio::test]
    async fn test_try_merge_branch_history() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().get_pr_fn = Box::new(|pr| {
            Ok(PRBuilder::default()
                .number(pr.0)
                .base(BranchBuilder::default().sha("main1".to_string()).create())
                .head(BranchBuilder::default().sha("head1".to_string()).create())
                .create())
        });

        state.comment("@bors try").await;
        state
            .client()
            .check_branch_history(TRY_MERGE_BRANCH_NAME, &["main1", &default_merge_sha()]);
        state
            .client()
            .check_branch_history(TRY_BRANCH_NAME, &[&default_merge_sha()]);
    }

    #[tokio::test]
    async fn test_try_merge_conflict() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().merge_branches_fn = Box::new(|| Err(MergeError::Conflict));
        state.comment("@bors try").await;

        insta::assert_snapshot!(state.client().get_comment(default_pr_number(), 0), @r###"
        :lock: Merge conflict

        This pull request and the master branch diverged in a way that cannot
         be automatically merged. Please rebase on top of the latest master
         branch, and let the reviewer approve again.

        <details><summary>How do I rebase?</summary>

        Assuming `self` is your fork and `upstream` is this repository,
         you can resolve the conflict following these steps:

        1. `git checkout pr-1` *(switch to your branch)*
        2. `git fetch upstream master` *(retrieve the latest master)*
        3. `git rebase upstream/master -p` *(rebase on top of it)*
        4. Follow the on-screen instruction to resolve conflicts (check `git status` if you got lost).
        5. `git push self pr-1 --force-with-lease` *(update this PR)*

        You may also read
         [*Git Rebasing to Resolve Conflicts* by Drew Blessing](http://blessing.io/git/git-rebase/open-source/2015/08/23/git-rebasing-to-resolve-conflicts.html)
         for a short tutorial.

        Please avoid the ["**Resolve conflicts**" button](https://help.github.com/articles/resolving-a-merge-conflict-on-github/) on GitHub.
         It uses `git merge` instead of `git rebase` which makes the PR commit history more difficult to read.

        Sometimes step 4 will complete without asking for resolution. This is usually due to difference between how `Cargo.lock` conflict is
        handled during merge and rebase. This is normal, and you should still perform step 5 to update this PR.

        </details>
        "###);
    }

    #[tokio::test]
    async fn test_try_merge_insert_into_db() {
        let mut state = ClientBuilder::default().create_state().await;
        state.comment("@bors try").await;

        assert!(state
            .db
            .find_build(
                &default_repo_name(),
                TRY_BRANCH_NAME.to_string(),
                CommitSha(default_merge_sha().to_string())
            )
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn test_try_merge_active_build() {
        let mut state = ClientBuilder::default().create_state().await;

        state.comment("@bors try").await;
        state.comment("@bors try").await;

        insta::assert_snapshot!(state.client().get_last_comment(default_pr_number()), @":exclamation: A try build is currently in progress. You can cancel it using @bors try cancel.");
    }

    #[tokio::test]
    async fn test_try_again_after_checks_finish() {
        let mut state = ClientBuilder::default().create_state().await;
        state
            .client()
            .set_checks(&default_merge_sha(), &[suite_success()]);

        state.comment("@bors try").await;
        state
            .perform_workflow_events(
                1,
                TRY_BRANCH_NAME,
                &default_merge_sha(),
                WorkflowStatus::Success,
            )
            .await;
        state.client().check_comment_count(default_pr_number(), 2);

        state.client().merge_branches_fn = Box::new(|| Ok(CommitSha("merge2".to_string())));
        state.comment("@bors try").await;
        insta::assert_snapshot!(state.client().get_last_comment(default_pr_number()), @":hourglass: Trying commit pr-sha with merge merge2…");

        state.client().set_checks("merge2", &[suite_success()]);
        state
            .perform_workflow_events(2, TRY_BRANCH_NAME, "merge2", WorkflowStatus::Success)
            .await;
        insta::assert_snapshot!(state.client().get_last_comment(default_pr_number()), @r###"
        :sunny: Try build successful
        - [workflow-2](https://workflow-2.com) :white_check_mark:
        Build commit: merge2 (`merge2`)
        "###);
    }

    #[tokio::test]
    async fn test_try_cancel_no_running_build() {
        let mut state = ClientBuilder::default().create_state().await;

        state.comment("@bors try cancel").await;

        insta::assert_snapshot!(state.client().get_last_comment(default_pr_number()), @":exclamation: There is currently no try build in progress.");
    }

    #[tokio::test]
    async fn test_try_cancel_running_build() {
        let mut state = ClientBuilder::default().create_state().await;

        state.comment("@bors try").await;
        state.comment("@bors try cancel").await;

        insta::assert_snapshot!(state.client().get_last_comment(default_pr_number()), @"Try build cancelled.");

        let build = state
            .db
            .find_build(
                &default_repo_name(),
                TRY_BRANCH_NAME.to_string(),
                default_merge_sha().into(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(build.status, BuildStatus::Cancelled);
    }

    #[tokio::test]
    async fn test_try_cancel_cancel_workflows() {
        let mut state = ClientBuilder::default().create_state().await;

        state.comment("@bors try").await;
        state
            .workflow_started(
                WorkflowStartedBuilder::default()
                    .branch(TRY_BRANCH_NAME.to_string())
                    .run_id(123),
            )
            .await;
        state
            .workflow_started(
                WorkflowStartedBuilder::default()
                    .branch(TRY_BRANCH_NAME.to_string())
                    .run_id(124),
            )
            .await;
        state.comment("@bors try cancel").await;
        state.client().check_cancelled_workflows(&[123, 124]);
    }

    #[tokio::test]
    async fn test_try_cancel_ignore_finished_workflows() {
        let mut state = ClientBuilder::default().create_state().await;
        state.client().set_checks(
            &default_merge_sha(),
            &[suite_success(), suite_failure(), suite_pending()],
        );

        state.comment("@bors try").await;
        state
            .perform_workflow_events(
                1,
                TRY_BRANCH_NAME,
                &default_merge_sha(),
                WorkflowStatus::Success,
            )
            .await;
        state
            .perform_workflow_events(
                2,
                TRY_BRANCH_NAME,
                &default_merge_sha(),
                WorkflowStatus::Failure,
            )
            .await;
        state
            .workflow_started(
                WorkflowStartedBuilder::default()
                    .branch(TRY_BRANCH_NAME.to_string())
                    .run_id(123),
            )
            .await;
        state.comment("@bors try cancel").await;
        state.client().check_cancelled_workflows(&[123]);
    }

    #[tokio::test]
    async fn test_try_cancel_ignore_external_workflows() {
        let mut state = ClientBuilder::default().create_state().await;
        state
            .client()
            .set_checks(&default_merge_sha(), &[suite_success()]);

        state.comment("@bors try").await;
        state
            .workflow_started(
                WorkflowStartedBuilder::default()
                    .branch(TRY_BRANCH_NAME.to_string())
                    .run_id(123)
                    .workflow_type(WorkflowType::External),
            )
            .await;
        state.comment("@bors try cancel").await;
        state.client().check_cancelled_workflows(&[]);
    }

    #[tokio::test]
    async fn test_try_workflow_start_after_cancel() {
        let mut state = ClientBuilder::default().create_state().await;
        state
            .client()
            .set_checks(&default_merge_sha(), &[suite_success()]);

        state.comment("@bors try").await;
        state.comment("@bors try cancel").await;
        state
            .workflow_started(
                WorkflowStartedBuilder::default()
                    .branch(TRY_BRANCH_NAME.to_string())
                    .run_id(123),
            )
            .await;
        assert_eq!(
            workflow::Entity::find()
                .all(state.db.connection())
                .await
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn test_try_build_start_modify_labels() {
        let mut state = ClientBuilder::default()
            .config(
                RepoConfigBuilder::default()
                    .add_label(LabelTrigger::TryBuildStarted, "foo")
                    .add_label(LabelTrigger::TryBuildStarted, "bar")
                    .remove_label(LabelTrigger::TryBuildStarted, "baz"),
            )
            .create_state()
            .await;

        state.comment("@bors try").await;
        state
            .client()
            .check_added_labels(default_pr_number(), &["foo", "bar"])
            .check_removed_labels(default_pr_number(), &["baz"]);
    }

    #[tokio::test]
    async fn test_try_build_succeeded_modify_labels() {
        let mut state = ClientBuilder::default()
            .config(
                RepoConfigBuilder::default()
                    .add_label(LabelTrigger::TryBuildSucceeded, "foo")
                    .add_label(LabelTrigger::TryBuildSucceeded, "bar")
                    .remove_label(LabelTrigger::TryBuildSucceeded, "baz"),
            )
            .create_state()
            .await;
        state
            .client()
            .set_checks(&default_merge_sha(), &[suite_success()]);

        state.comment("@bors try").await;
        state
            .perform_workflow_events(
                1,
                TRY_BRANCH_NAME,
                &default_merge_sha(),
                WorkflowStatus::Success,
            )
            .await;
        state
            .client()
            .check_added_labels(default_pr_number(), &["foo", "bar"])
            .check_removed_labels(default_pr_number(), &["baz"]);
    }

    #[tokio::test]
    async fn test_try_build_failed_modify_labels() {
        let mut state = ClientBuilder::default()
            .config(
                RepoConfigBuilder::default()
                    .add_label(LabelTrigger::TryBuildFailed, "foo")
                    .add_label(LabelTrigger::TryBuildFailed, "bar")
                    .remove_label(LabelTrigger::TryBuildFailed, "baz"),
            )
            .create_state()
            .await;
        state
            .client()
            .set_checks(&default_merge_sha(), &[suite_failure()]);

        state.comment("@bors try").await;
        state
            .perform_workflow_events(
                1,
                TRY_BRANCH_NAME,
                &default_merge_sha(),
                WorkflowStatus::Failure,
            )
            .await;
        state
            .client()
            .check_added_labels(default_pr_number(), &["foo", "bar"])
            .check_removed_labels(default_pr_number(), &["baz"]);
    }
}
