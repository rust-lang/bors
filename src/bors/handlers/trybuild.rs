use std::sync::Arc;

use anyhow::{anyhow, Context};

use crate::bors::command::Parent;
use crate::bors::handlers::labels::handle_label_trigger;
use crate::bors::Comment;
use crate::bors::RepositoryClient;
use crate::bors::RepositoryState;
use crate::database::RunId;
use crate::database::{
    BuildModel, BuildStatus, DbClient, PullRequestModel, WorkflowStatus, WorkflowType,
};
use crate::github::{
    CommitSha, GithubUser, LabelTrigger, MergeError, PullRequest, PullRequestNumber,
};
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
///
/// If `parent` is set, it will use it as a base commit for the merge.
/// Otherwise, it will use the latest commit on the main repository branch.
pub(super) async fn command_try_build<Client: RepositoryClient>(
    repo: Arc<RepositoryState<Client>>,
    db: Arc<dyn DbClient>,
    pr: &PullRequest,
    author: &GithubUser,
    parent: Option<Parent>,
    jobs: Vec<String>,
) -> anyhow::Result<()> {
    let repo = repo.as_ref();
    if !check_try_permissions(repo, pr, author).await? {
        return Ok(());
    }

    let pr_model = db
        .get_or_create_pull_request(repo.client.repository(), pr.number)
        .await
        .context("Cannot find or create PR")?;

    let last_parent = if let Some(ref build) = pr_model.try_build {
        if build.status == BuildStatus::Pending {
            tracing::warn!("Try build already in progress");
            repo.client
                .post_comment(pr.number, Comment::new(":exclamation: A try build is currently in progress. You can cancel it using @bors try cancel.".to_string()))
                .await?;
            return Ok(());
        }
        Some(CommitSha(build.parent.clone()))
    } else {
        None
    };

    let base_sha = match parent.clone() {
        Some(parent) => match parent {
            Parent::Last => match last_parent {
                None => {
                    repo.client
                        .post_comment(pr.number, Comment::new(":exclamation: There was no previous build. Please set an explicit parent or remove the `parent=last` argument to use the default parent.".to_string()))
                        .await?;
                    return Ok(());
                }
                Some(last_parent) => last_parent,
            },
            Parent::CommitSha(parent) => parent,
        },
        None => repo
            .client
            .get_branch_sha(&pr.base.name)
            .await
            .with_context(|| format!("Cannot get SHA for branch {}", pr.base.name))?,
    };

    tracing::debug!("Attempting to merge with base SHA {base_sha}");

    // First set the try branch to our base commit (either the selected parent or the main branch).
    repo.client
        .set_branch_to_sha(TRY_MERGE_BRANCH_NAME, &base_sha)
        .await
        .map_err(|error| anyhow!("Cannot set try merge branch to {}: {error:?}", base_sha.0))?;

    // Then merge the PR commit into the try branch
    match repo
        .client
        .merge_branches(
            TRY_MERGE_BRANCH_NAME,
            &pr.head.sha,
            &auto_merge_commit_message(pr, "<try>", jobs),
        )
        .await
    {
        Ok(merge_sha) => {
            tracing::debug!("Merge successful, SHA: {merge_sha}");
            // If the merge was succesful, then set the actual try branch that will run CI to the
            // merged commit.
            repo.client
                .set_branch_to_sha(TRY_BRANCH_NAME, &merge_sha)
                .await
                .map_err(|error| anyhow!("Cannot set try branch to main branch: {error:?}"))?;

            db.attach_try_build(
                pr_model,
                TRY_BRANCH_NAME.to_string(),
                merge_sha.clone(),
                base_sha.clone(),
            )
            .await?;
            tracing::info!("Try build started");

            handle_label_trigger(repo, pr.number, LabelTrigger::TryBuildStarted).await?;

            let comment = Comment::new(format!(
                ":hourglass: Trying commit {} with merge {}…",
                pr.head.sha.clone(),
                merge_sha
            ));
            repo.client.post_comment(pr.number, comment).await?;
            Ok(())
        }
        Err(MergeError::Conflict) => {
            tracing::warn!("Merge conflict");
            repo.client
                .post_comment(
                    pr.number,
                    Comment::new(merge_conflict_message(&pr.head.name)),
                )
                .await?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) async fn command_try_cancel<Client: RepositoryClient>(
    repo: Arc<RepositoryState<Client>>,
    db: Arc<dyn DbClient>,
    pr: &PullRequest,
    author: &GithubUser,
) -> anyhow::Result<()> {
    let repo = repo.as_ref();
    if !check_try_permissions(repo, pr, author).await? {
        return Ok(());
    }

    let pr_number: PullRequestNumber = pr.number;
    let pr = db
        .get_or_create_pull_request(repo.client.repository(), pr_number)
        .await?;

    let Some(build) = get_pending_build(pr) else {
        tracing::warn!("No build found");
        repo.client
            .post_comment(
                pr_number,
                Comment::new(
                    ":exclamation: There is currently no try build in progress.".to_string(),
                ),
            )
            .await?;
        return Ok(());
    };

    match cancel_build_workflows(repo, db.as_ref(), &build).await {
        Err(error) => {
            tracing::error!(
                "Could not cancel workflows for SHA {}: {error:?}",
                build.commit_sha
            );
            db.update_build_status(&build, BuildStatus::Cancelled)
                .await?;
            repo.client
                .post_comment(
                    pr_number,
                    Comment::new(
                        "Try build was cancelled. It was not possible to cancel some workflows."
                            .to_string(),
                    ),
                )
                .await?
        }
        Ok(workflow_ids) => {
            db.update_build_status(&build, BuildStatus::Cancelled)
                .await?;
            tracing::info!("Try build cancelled");

            let mut try_build_cancelled_comment = r#"Try build cancelled.
Cancelled workflows:"#
                .to_string();
            for id in workflow_ids {
                let url = repo.client.get_workflow_url(id);
                try_build_cancelled_comment += format!("\n- {}", url).as_str();
            }

            repo.client
                .post_comment(pr_number, Comment::new(try_build_cancelled_comment))
                .await?
        }
    };

    Ok(())
}

pub async fn cancel_build_workflows<Client: RepositoryClient>(
    repo: &RepositoryState<Client>,
    db: &dyn DbClient,
    build: &BuildModel,
) -> anyhow::Result<Vec<RunId>> {
    let pending_workflows = db
        .get_workflows_for_build(build)
        .await?
        .into_iter()
        .filter(|w| w.status == WorkflowStatus::Pending && w.workflow_type == WorkflowType::Github)
        .map(|w| w.run_id)
        .collect::<Vec<_>>();

    tracing::info!("Cancelling workflows {:?}", pending_workflows);
    repo.client.cancel_workflows(&pending_workflows).await?;
    Ok(pending_workflows)
}

fn get_pending_build(pr: PullRequestModel) -> Option<BuildModel> {
    pr.try_build
        .and_then(|b| (b.status == BuildStatus::Pending).then_some(b))
}

fn auto_merge_commit_message(pr: &PullRequest, reviewer: &str, jobs: Vec<String>) -> String {
    let pr_number = pr.number;
    let mut message = format!(
        r#"Auto merge of #{pr_number} - {pr_label}, r={reviewer}
{pr_title}

{pr_message}"#,
        pr_label = pr.head_label,
        pr_title = pr.title,
        pr_message = pr.message
    );

    // if jobs is empty, try-job won't be added to the message
    for job in jobs {
        message.push_str(&format!("\ntry-job: {}", job));
    }
    message
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
    repo: &RepositoryState<Client>,
    pr: &PullRequest,
    author: &GithubUser,
) -> anyhow::Result<bool> {
    let result = if !repo
        .permissions
        .load()
        .has_permission(author.id, PermissionType::Try)
    {
        tracing::warn!("Try permission denied for {}", author.username);
        repo.client
            .post_comment(
                pr.number,
                Comment::new(format!(
                    "@{}: :key: Insufficient privileges: not in try users",
                    author.username
                )),
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
    use crate::bors::handlers::trybuild::{TRY_BRANCH_NAME, TRY_MERGE_BRANCH_NAME};
    use crate::database::{BuildStatus, DbClient, WorkflowStatus, WorkflowType};
    use crate::github::{CommitSha, LabelTrigger, MergeError, PullRequestNumber};
    use crate::tests::database::MockedDBClient;
    use crate::tests::event::{
        default_pr_number, suite_failure, suite_pending, suite_success, WorkflowStartedBuilder,
    };
    use crate::tests::github::{BranchBuilder, PRBuilder};
    use crate::tests::mocks::{default_repo_name, BorsBuilder, Permissions, World};
    use crate::tests::state::{default_merge_sha, ClientBuilder, RepoConfigBuilder};

    #[sqlx::test]
    async fn try_no_permissions(pool: sqlx::PgPool) {
        let mut world = World::default();
        world.get_repo(default_repo_name()).lock().permissions = Permissions::default();

        BorsBuilder::new(pool)
            .world(world)
            .run_test(|mut tester| async {
                tester.post_comment("@bors try").await;
                assert_eq!(
                    tester.get_comment().await,
                    "@default-user: :key: Insufficient privileges: not in try users"
                );
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn test_try_merge_comment(pool: sqlx::PgPool) {
        let world = World::default();
        BorsBuilder::new(pool)
            .world(world)
            .run_test(|mut tester| async {
                tester.post_comment("@bors try").await;
                // assert_eq!(
                //     tester.get_comment().await,
                //     ":hourglass: Trying commit head1 with merge sha-merged…"
                // );
                Ok(tester)
            })
            .await;

        // let state = ClientBuilder::default().pool(pool).create_state().await;
        // state.client().set_get_pr_fn(|pr| {
        //     Ok(PRBuilder::default()
        //         .number(pr.0)
        //         .head(BranchBuilder::default().sha("head1".to_string()).create())
        //         .create())
        // });
        //
        // state.comment("@bors try").await;
        //
        // state.client().check_comments(
        //     default_pr_number(),
        //     &[":hourglass: Trying commit head1 with merge sha-merged…"],
        // );
    }

    #[sqlx::test]
    async fn test_try_merge_unknown_base_branch(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;
        state.client().set_get_pr_fn(|pr| {
            Ok(PRBuilder::default()
                .number(pr.0)
                .base(
                    BranchBuilder::default()
                        .name("nonexistent".to_string())
                        .create(),
                )
                .head(BranchBuilder::default().sha("head1".to_string()).create())
                .create())
        });

        state.comment("@bors try").await;

        state.client().check_comments(
            default_pr_number(),
            &[":x: Encountered an error while executing command"],
        );
    }

    #[sqlx::test]
    async fn test_try_merge_branch_history(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;
        let main_sha = "main1-sha";
        let main_name = "main1";

        state.client().set_get_pr_fn(|pr| {
            Ok(PRBuilder::default()
                .number(pr.0)
                .base(
                    BranchBuilder::default()
                        .sha(main_sha.to_string())
                        .name(main_name.to_string())
                        .create(),
                )
                .head(BranchBuilder::default().sha("head1".to_string()).create())
                .create())
        });

        state.client().add_branch_sha(main_name, main_sha);

        state.comment("@bors try").await;
        state
            .client()
            .check_branch_history(TRY_MERGE_BRANCH_NAME, &[main_sha, &default_merge_sha()]);
        state
            .client()
            .check_branch_history(TRY_BRANCH_NAME, &[&default_merge_sha()]);
    }

    #[sqlx::test]
    async fn test_try_merge_explicit_parent(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;
        state
            .comment("@bors try parent=ea9c1b050cc8b420c2c211d2177811e564a4dc60")
            .await;
        state.client().check_branch_history(
            TRY_MERGE_BRANCH_NAME,
            &[
                "ea9c1b050cc8b420c2c211d2177811e564a4dc60",
                &default_merge_sha(),
            ],
        );
    }

    #[sqlx::test]
    async fn test_try_merge_last_parent(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;

        state
            .comment("@bors try parent=ea9c1b050cc8b420c2c211d2177811e564a4dc60")
            .await;
        state.client().check_branch_history(
            TRY_MERGE_BRANCH_NAME,
            &[
                "ea9c1b050cc8b420c2c211d2177811e564a4dc60",
                &default_merge_sha(),
            ],
        );
        state
            .perform_workflow_events(
                1,
                TRY_BRANCH_NAME,
                &default_merge_sha(),
                WorkflowStatus::Success,
            )
            .await;
        state.client().check_comment_count(default_pr_number(), 2);
        state.comment("@bors try parent=last").await;
        state.client().check_branch_history(
            TRY_MERGE_BRANCH_NAME,
            &[
                "ea9c1b050cc8b420c2c211d2177811e564a4dc60",
                &default_merge_sha(),
                "ea9c1b050cc8b420c2c211d2177811e564a4dc60",
                &default_merge_sha(),
            ],
        );
    }

    #[sqlx::test]
    async fn test_try_merge_last_parent_unknown(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;

        state.comment("@bors try parent=last").await;
        state.client().check_comments(
            default_pr_number(),
            &[":exclamation: There was no previous build. Please set an explicit parent or remove the `parent=last` argument to use the default parent."],
        );
    }

    #[sqlx::test]
    async fn test_try_merge_conflict(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;
        state
            .client()
            .set_merge_branches_fn(Box::new(|| Err(MergeError::Conflict)));
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

    #[sqlx::test]
    async fn test_try_merge_insert_into_db(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;
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

    #[sqlx::test]
    async fn test_try_merge_active_build(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;

        state.comment("@bors try").await;
        state.comment("@bors try").await;

        insta::assert_snapshot!(state.client().get_last_comment(default_pr_number()), @":exclamation: A try build is currently in progress. You can cancel it using @bors try cancel.");
    }

    #[sqlx::test]
    async fn test_try_again_after_checks_finish(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;
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
        state
            .client()
            .set_merge_branches_fn(|| Ok(CommitSha("merge2".to_string())));

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
        <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge2"} -->
        "###);
    }

    #[sqlx::test]
    async fn test_try_cancel_no_running_build(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;

        state.comment("@bors try cancel").await;

        insta::assert_snapshot!(state.client().get_last_comment(default_pr_number()), @":exclamation: There is currently no try build in progress.");
    }

    #[sqlx::test]
    async fn test_try_cancel_cancel_workflows(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;

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

        insta::assert_snapshot!(state.client().get_last_comment(default_pr_number()), @r###"
        Try build cancelled.
        Cancelled workflows:
        - https://github.com/owner/name/actions/runs/123
        - https://github.com/owner/name/actions/runs/124
        "###);
    }

    #[sqlx::test]
    async fn test_try_cancel_error(pool: sqlx::PgPool) {
        let mut db = MockedDBClient::new(pool);
        db.get_workflows_for_build = Some(Box::new(|| Err(anyhow::anyhow!("Errr"))));
        let state = ClientBuilder::default().db(db).create_state().await;

        state.comment("@bors try").await;
        state.comment("@bors try cancel").await;
        let name = default_repo_name();
        let pr = state
            .db
            .get_or_create_pull_request(&name, PullRequestNumber(default_pr_number()))
            .await
            .unwrap();
        let build = pr.try_build.unwrap();
        assert_eq!(build.status, BuildStatus::Cancelled);

        insta::assert_snapshot!(state.client().get_last_comment(default_pr_number()), @"Try build was cancelled. It was not possible to cancel some workflows.");
    }

    #[sqlx::test]
    async fn test_try_cancel_ignore_finished_workflows(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;
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

    #[sqlx::test]
    async fn test_try_cancel_ignore_external_workflows(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;
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

    #[sqlx::test]
    async fn test_try_workflow_start_after_cancel(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;
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
        assert_eq!(state.db.get_all_workflows().await.unwrap().len(), 0);
    }

    #[sqlx::test]
    async fn test_try_build_start_modify_labels(pool: sqlx::PgPool) {
        let state = ClientBuilder::default()
            .config(
                RepoConfigBuilder::default()
                    .add_label(LabelTrigger::TryBuildStarted, "foo")
                    .add_label(LabelTrigger::TryBuildStarted, "bar")
                    .remove_label(LabelTrigger::TryBuildStarted, "baz"),
            )
            .pool(pool)
            .create_state()
            .await;

        state.comment("@bors try").await;
        state
            .client()
            .check_added_labels(default_pr_number(), &["foo", "bar"])
            .check_removed_labels(default_pr_number(), &["baz"]);
    }

    #[sqlx::test]
    async fn test_try_build_succeeded_modify_labels(pool: sqlx::PgPool) {
        let state = ClientBuilder::default()
            .config(
                RepoConfigBuilder::default()
                    .add_label(LabelTrigger::TryBuildSucceeded, "foo")
                    .add_label(LabelTrigger::TryBuildSucceeded, "bar")
                    .remove_label(LabelTrigger::TryBuildSucceeded, "baz"),
            )
            .pool(pool)
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

    #[sqlx::test]
    async fn test_try_build_failed_modify_labels(pool: sqlx::PgPool) {
        let state = ClientBuilder::default()
            .config(
                RepoConfigBuilder::default()
                    .add_label(LabelTrigger::TryBuildFailed, "foo")
                    .add_label(LabelTrigger::TryBuildFailed, "bar")
                    .remove_label(LabelTrigger::TryBuildFailed, "baz"),
            )
            .pool(pool)
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
