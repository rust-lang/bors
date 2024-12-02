use std::sync::Arc;

use anyhow::{anyhow, Context};

use crate::bors::command::Parent;
use crate::bors::comment::cant_find_last_parent_comment;
use crate::bors::comment::no_try_build_in_progress_comment;
use crate::bors::comment::try_build_cancelled_comment;
use crate::bors::comment::try_build_in_progress_comment;
use crate::bors::comment::unclean_try_build_cancelled_comment;
use crate::bors::handlers::labels::handle_label_trigger;
use crate::bors::Comment;
use crate::bors::RepositoryState;
use crate::database::RunId;
use crate::database::{BuildModel, BuildStatus, PullRequestModel};
use crate::github::api::client::GithubRepositoryClient;
use crate::github::GithubRepoName;
use crate::github::{
    CommitSha, GithubUser, LabelTrigger, MergeError, PullRequest, PullRequestNumber,
};
use crate::permissions::PermissionType;
use crate::PgDbClient;

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
pub(super) async fn command_try_build(
    repo: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: &PullRequest,
    author: &GithubUser,
    parent: Option<Parent>,
    jobs: Vec<String>,
) -> anyhow::Result<()> {
    let repo = repo.as_ref();
    if !check_try_permissions(repo, pr, author).await? {
        return Ok(());
    }

    // Create pr model based on CI repo, so we can retrieve the pr later when
    // the CI repo emits events
    let pr_model = db
        .get_or_create_pull_request(repo.ci_client.repository(), pr.number)
        .await
        .context("Cannot find or create PR")?;

    let base_sha = match get_base_sha(&pr_model, parent) {
        Ok(Some(base_sha)) => base_sha,
        Ok(None) => repo
            .client
            .get_branch_sha(&pr.base.name)
            .await
            .context(format!("Cannot get SHA for branch {}", pr.base.name))?,
        Err(comment) => {
            repo.client.post_comment(pr.number, comment).await?;
            return Ok(());
        }
    };

    match attempt_merge(
        &repo.ci_client,
        &pr.head.sha,
        &base_sha,
        &auto_merge_commit_message(pr, repo.client.repository(), "<try>", jobs),
    )
    .await?
    {
        MergeResult::Success(merge_sha) => {
            // If the merge was succesful, run CI with merged commit
            run_try_build(&repo.ci_client, &db, pr_model, merge_sha.clone(), base_sha).await?;

            handle_label_trigger(repo, pr.number, LabelTrigger::TryBuildStarted).await?;

            repo.client
                .post_comment(pr.number, trying_build_comment(&pr.head.sha, &merge_sha))
                .await
        }
        MergeResult::Conflict => {
            repo.client
                .post_comment(pr.number, merge_conflict_comment(&pr.head.name))
                .await
        }
    }
}

async fn attempt_merge(
    ci_client: &GithubRepositoryClient,
    head_sha: &CommitSha,
    base_sha: &CommitSha,
    merge_message: &str,
) -> anyhow::Result<MergeResult> {
    tracing::debug!("Attempting to merge with base SHA {base_sha}");

    // First set the try branch to our base commit (either the selected parent or the main branch).
    ci_client
        .set_branch_to_sha(TRY_MERGE_BRANCH_NAME, base_sha)
        .await
        .map_err(|error| anyhow!("Cannot set try merge branch to {}: {error:?}", base_sha.0))?;

    // Then merge the PR commit into the try branch
    match ci_client
        .merge_branches(TRY_MERGE_BRANCH_NAME, head_sha, merge_message)
        .await
    {
        Ok(merge_sha) => {
            tracing::debug!("Merge successful, SHA: {merge_sha}");

            Ok(MergeResult::Success(merge_sha))
        }
        Err(MergeError::Conflict) => {
            tracing::warn!("Merge conflict");

            Ok(MergeResult::Conflict)
        }
        Err(error) => Err(error.into()),
    }
}

async fn run_try_build(
    ci_client: &GithubRepositoryClient,
    db: &PgDbClient,
    pr_model: PullRequestModel,
    commit_sha: CommitSha,
    parent_sha: CommitSha,
) -> anyhow::Result<()> {
    ci_client
        .set_branch_to_sha(TRY_BRANCH_NAME, &commit_sha)
        .await
        .map_err(|error| anyhow!("Cannot set try branch to main branch: {error:?}"))?;

    db.attach_try_build(
        pr_model,
        TRY_BRANCH_NAME.to_string(),
        commit_sha,
        parent_sha,
    )
    .await?;

    tracing::info!("Try build started");
    Ok(())
}

enum MergeResult {
    Success(CommitSha),
    Conflict,
}

fn get_base_sha(
    pr_model: &PullRequestModel,
    parent: Option<Parent>,
) -> Result<Option<CommitSha>, Comment> {
    let last_parent = if let Some(ref build) = pr_model.try_build {
        if build.status == BuildStatus::Pending {
            tracing::warn!("Try build already in progress");
            return Err(try_build_in_progress_comment());
        } else {
            Some(CommitSha(build.parent.clone()))
        }
    } else {
        None
    };

    match parent.clone() {
        Some(parent) => match parent {
            Parent::Last => match last_parent {
                None => Err(cant_find_last_parent_comment()),
                Some(last_parent) => Ok(Some(last_parent)),
            },
            Parent::CommitSha(parent) => Ok(Some(parent)),
        },
        None => Ok(None),
    }
}

pub(super) async fn command_try_cancel(
    repo: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: &PullRequest,
    author: &GithubUser,
) -> anyhow::Result<()> {
    let repo = repo.as_ref();
    if !check_try_permissions(repo, pr, author).await? {
        return Ok(());
    }

    let pr_number: PullRequestNumber = pr.number;
    let pr = db
        .get_or_create_pull_request(repo.ci_client.repository(), pr_number)
        .await?;

    let Some(build) = get_pending_build(pr) else {
        tracing::warn!("No build found");
        repo.client
            .post_comment(pr_number, no_try_build_in_progress_comment())
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
                .post_comment(pr_number, unclean_try_build_cancelled_comment())
                .await?
        }
        Ok(workflow_ids) => {
            db.update_build_status(&build, BuildStatus::Cancelled)
                .await?;
            tracing::info!("Try build cancelled");

            repo.client
                .post_comment(
                    pr_number,
                    try_build_cancelled_comment(
                        repo.ci_client.get_workflow_urls(workflow_ids.into_iter()),
                    ),
                )
                .await?
        }
    };

    Ok(())
}

pub async fn cancel_build_workflows(
    repo: &RepositoryState,
    db: &PgDbClient,
    build: &BuildModel,
) -> anyhow::Result<Vec<RunId>> {
    let pending_workflows = db.get_pending_workflows_for_build(build).await?;

    tracing::info!("Cancelling workflows {:?}", pending_workflows);
    repo.ci_client.cancel_workflows(&pending_workflows).await?;
    Ok(pending_workflows)
}

fn get_pending_build(pr: PullRequestModel) -> Option<BuildModel> {
    pr.try_build
        .and_then(|b| (b.status == BuildStatus::Pending).then_some(b))
}

fn auto_merge_commit_message(
    pr: &PullRequest,
    name: &GithubRepoName,
    reviewer: &str,
    jobs: Vec<String>,
) -> String {
    let pr_number = pr.number;
    let mut message = format!(
        r#"Auto merge of {repo_owner}/{repo_name}#{pr_number} - {pr_label}, r={reviewer}
{pr_title}

{pr_message}"#,
        pr_label = pr.head_label,
        pr_title = pr.title,
        pr_message = pr.message,
        repo_owner = name.owner(),
        repo_name = name.name()
    );

    // if jobs is empty, try-job won't be added to the message
    for job in jobs {
        message.push_str(&format!("\ntry-job: {}", job));
    }
    message
}

fn trying_build_comment(head_sha: &CommitSha, merge_sha: &CommitSha) -> Comment {
    Comment::new(format!(
        ":hourglass: Trying commit {head_sha} with merge {merge_sha}…"
    ))
}

fn merge_conflict_comment(branch: &str) -> Comment {
    let message = format!(
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
    );
    Comment::new(message)
}

async fn check_try_permissions(
    repo: &RepositoryState,
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
    use crate::database::operations::get_all_workflows;
    use crate::database::BuildStatus;
    use crate::github::{CommitSha, PullRequestNumber};
    use crate::tests::mocks::{
        default_pr_number, default_repo_name, run_test, BorsBuilder, Permissions, Workflow,
        WorkflowEvent, World,
    };

    #[sqlx::test]
    async fn try_success(pool: sqlx::PgPool) {
        run_test(pool.clone(), |mut tester| async {
            tester.create_branch(TRY_BRANCH_NAME).expect_suites(1);
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            tester.workflow_success(tester.try_branch()).await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @r###"
            :sunny: Try build successful
            - [Workflow1](https://github.com/workflows/Workflow1/1) :white_check_mark:
            Build commit: merge-main-sha1-pr-1-sha-0 (`merge-main-sha1-pr-1-sha-0`)
            <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge-main-sha1-pr-1-sha-0"} -->
            "###
            );
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn try_failure(pool: sqlx::PgPool) {
        run_test(pool.clone(), |mut tester| async {
            tester.create_branch(TRY_BRANCH_NAME).expect_suites(1);
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            tester.workflow_failure(tester.try_branch()).await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @r###"
            :broken_heart: Test failed
            - [Workflow1](https://github.com/workflows/Workflow1/1) :x:
            "###
            );
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn try_no_permissions(pool: sqlx::PgPool) {
        let world = World::default();
        world.get_repo(default_repo_name()).lock().permissions = Permissions::default();

        BorsBuilder::new(pool)
            .world(world)
            .run_test(|mut tester| async {
                tester.post_comment("@bors try").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    "@default-user: :key: Insufficient privileges: not in try users"
                );
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn try_merge_comment(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors try").await?;
            assert_eq!(
                tester.get_comment().await?,
                ":hourglass: Trying commit pr-1-sha with merge merge-main-sha1-pr-1-sha-0…"
            );
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn try_merge_branch_history(pool: sqlx::PgPool) {
        let world = run_test(pool, |mut tester| async {
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            Ok(tester)
        })
        .await;
        world.check_sha_history(
            default_repo_name(),
            TRY_MERGE_BRANCH_NAME,
            &["main-sha1", "merge-main-sha1-pr-1-sha-0"],
        );
        world.check_sha_history(
            default_repo_name(),
            TRY_BRANCH_NAME,
            &["merge-main-sha1-pr-1-sha-0"],
        );
    }

    #[sqlx::test]
    async fn try_merge_explicit_parent(pool: sqlx::PgPool) {
        let world = run_test(pool, |mut tester| async {
            tester
                .post_comment("@bors try parent=ea9c1b050cc8b420c2c211d2177811e564a4dc60")
                .await?;
            tester.expect_comments(1).await;
            Ok(tester)
        })
        .await;
        world.check_sha_history(
            default_repo_name(),
            TRY_MERGE_BRANCH_NAME,
            &[
                "ea9c1b050cc8b420c2c211d2177811e564a4dc60",
                "merge-ea9c1b050cc8b420c2c211d2177811e564a4dc60-pr-1-sha-0",
            ],
        );
    }

    #[sqlx::test]
    async fn try_merge_last_parent(pool: sqlx::PgPool) {
        let world = run_test(pool, |mut tester| async {
            tester
                .post_comment("@bors try parent=ea9c1b050cc8b420c2c211d2177811e564a4dc60")
                .await?;
            tester.expect_comments(1).await;
            tester.workflow_success(tester.try_branch()).await?;
            tester.expect_comments(1).await;
            tester.post_comment("@bors try parent=last").await?;
            insta::assert_snapshot!(tester.get_comment().await?, @":hourglass: Trying commit pr-1-sha with merge merge-ea9c1b050cc8b420c2c211d2177811e564a4dc60-pr-1-sha-1…");
            Ok(tester)
        })
        .await;
        world.check_sha_history(
            default_repo_name(),
            TRY_MERGE_BRANCH_NAME,
            &[
                "ea9c1b050cc8b420c2c211d2177811e564a4dc60",
                "merge-ea9c1b050cc8b420c2c211d2177811e564a4dc60-pr-1-sha-0",
                "ea9c1b050cc8b420c2c211d2177811e564a4dc60",
                "merge-ea9c1b050cc8b420c2c211d2177811e564a4dc60-pr-1-sha-1",
            ],
        );
    }

    #[sqlx::test]
    async fn try_merge_last_parent_unknown(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester
                .post_comment("@bors try parent=last")
                .await?;
            insta::assert_snapshot!(tester.get_comment().await?, @":exclamation: There was no previous build. Please set an explicit parent or remove the `parent=last` argument to use the default parent.");
            Ok(tester)
        })
            .await;
    }

    #[sqlx::test]
    async fn try_merge_conflict(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.create_branch(TRY_MERGE_BRANCH_NAME)
                .merge_conflict = true;
            tester
                .post_comment("@bors try")
                .await?;
            insta::assert_snapshot!(tester.get_comment().await?, @r###"
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
            Ok(tester)
        })
            .await;
    }

    #[sqlx::test]
    async fn try_merge_insert_into_db(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            assert!(tester
                .db()
                .find_build(
                    &default_repo_name(),
                    TRY_BRANCH_NAME.to_string(),
                    CommitSha(tester.try_branch().get_sha().to_string()),
                )
                .await?
                .is_some());
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn try_merge_active_build(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            tester.post_comment("@bors try").await?;
            insta::assert_snapshot!(tester.get_comment().await?, @":exclamation: A try build is currently in progress. You can cancel it using @bors try cancel.");
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn try_again_after_checks_finish(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            tester
                .workflow_success(Workflow::from(tester.try_branch()).with_run_id(1))
                .await?;
            tester.expect_comments(1).await;

            tester.get_branch_mut(TRY_BRANCH_NAME).reset_suites();
            tester.post_comment("@bors try").await?;
            insta::assert_snapshot!(tester.get_comment().await?, @":hourglass: Trying commit pr-1-sha with merge merge-main-sha1-pr-1-sha-1…");
            tester
                .workflow_success(Workflow::from(tester.try_branch()).with_run_id(2))
                .await?;
            insta::assert_snapshot!(tester.get_comment().await?, @r###"
            :sunny: Try build successful
            - [Workflow1](https://github.com/workflows/Workflow1/2) :white_check_mark:
            Build commit: merge-main-sha1-pr-1-sha-1 (`merge-main-sha1-pr-1-sha-1`)
            <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge-main-sha1-pr-1-sha-1"} -->
            "###);
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn try_cancel_no_running_build(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors try cancel").await?;
            insta::assert_snapshot!(tester.get_comment().await?, @":exclamation: There is currently no try build in progress.");
            Ok(tester)
        })
            .await;
    }

    #[sqlx::test]
    async fn try_cancel_cancel_workflows(pool: sqlx::PgPool) {
        let world = run_test(pool, |mut tester| async {
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;

            let branch = tester.try_branch();
            tester
                .workflow_event(WorkflowEvent::started(
                    Workflow::from(branch.clone()).with_run_id(123),
                ))
                .await?;
            tester
                .workflow_event(WorkflowEvent::started(
                    Workflow::from(branch.clone()).with_run_id(124),
                ))
                .await?;
            tester.post_comment("@bors try cancel").await?;
            insta::assert_snapshot!(tester.get_comment().await?, @r###"
            Try build cancelled.
            Cancelled workflows:
            - https://github.com/rust-lang/borstest/actions/runs/123
            - https://github.com/rust-lang/borstest/actions/runs/124
            "###);
            Ok(tester)
        })
        .await;
        world.check_cancelled_workflows(default_repo_name(), &[123, 124]);
    }

    #[sqlx::test]
    async fn try_cancel_error(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.default_repo().lock().workflow_cancel_error = true;
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            tester
                .workflow_event(WorkflowEvent::started(tester.try_branch()))
                .await?;
            tester.post_comment("@bors try cancel").await?;
            insta::assert_snapshot!(tester.get_comment().await?, @"Try build was cancelled. It was not possible to cancel some workflows.");
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn try_cancel_cancel_in_db(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.default_repo().lock().workflow_cancel_error = true;
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            tester.post_comment("@bors try cancel").await?;
            tester.expect_comments(1).await;
            let pr = tester
                .db()
                .get_or_create_pull_request(
                    &default_repo_name(),
                    PullRequestNumber(default_pr_number()),
                )
                .await?;
            assert_eq!(pr.try_build.unwrap().status, BuildStatus::Cancelled);
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn try_cancel_ignore_finished_workflows(pool: sqlx::PgPool) {
        let world = run_test(pool, |mut tester| async {
            tester.create_branch(TRY_BRANCH_NAME).expect_suites(3);
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;

            let branch = tester.try_branch();
            tester
                .workflow_success(Workflow::from(branch.clone()).with_run_id(1))
                .await?;
            tester
                .workflow_failure(Workflow::from(branch.clone()).with_run_id(2))
                .await?;
            tester
                .workflow_event(WorkflowEvent::started(
                    Workflow::from(branch).with_run_id(3),
                ))
                .await?;
            tester.post_comment("@bors try cancel").await?;
            tester.expect_comments(1).await;
            Ok(tester)
        })
        .await;
        world.check_cancelled_workflows(default_repo_name(), &[3]);
    }

    #[sqlx::test]
    async fn try_cancel_ignore_external_workflows(pool: sqlx::PgPool) {
        let world = run_test(pool, |mut tester| async {
            tester.create_branch(TRY_BRANCH_NAME).expect_suites(1);
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            tester
                .workflow_success(Workflow::from(tester.try_branch()).make_external())
                .await?;
            tester.post_comment("@bors try cancel").await?;
            tester.expect_comments(1).await;
            Ok(tester)
        })
        .await;
        world.check_cancelled_workflows(default_repo_name(), &[]);
    }

    #[sqlx::test]
    async fn try_workflow_start_after_cancel(pool: sqlx::PgPool) {
        run_test(pool.clone(), |mut tester| async {
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            tester.post_comment("@bors try cancel").await?;
            tester.expect_comments(1).await;

            tester
                .workflow_event(WorkflowEvent::started(tester.try_branch()))
                .await?;
            Ok(tester)
        })
        .await;
        assert_eq!(get_all_workflows(&pool).await.unwrap().len(), 0);
    }

    #[sqlx::test]
    async fn try_build_start_modify_labels(pool: sqlx::PgPool) {
        let world = World::default();
        world.default_repo().lock().set_config(
            r#"
[labels]
try = ["+foo", "+bar", "-baz"]
"#,
        );
        BorsBuilder::new(pool)
            .world(world)
            .run_test(|mut tester| async {
                tester.post_comment("@bors try").await?;
                insta::assert_snapshot!(tester.get_comment().await?, @":hourglass: Trying commit pr-1-sha with merge merge-main-sha1-pr-1-sha-0…");
                let repo = tester.default_repo();
                let pr = repo.lock().get_pr(default_pr_number()).clone();
                pr.check_added_labels(&["foo", "bar"]);
                pr.check_removed_labels(&["baz"]);
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn try_build_succeeded_modify_labels(pool: sqlx::PgPool) {
        let world = World::default();
        world.default_repo().lock().set_config(
            r#"
[labels]
try_succeed = ["+foo", "+bar", "-baz"]
"#,
        );
        BorsBuilder::new(pool)
            .world(world)
            .run_test(|mut tester| async {
                tester.post_comment("@bors try").await?;
                insta::assert_snapshot!(tester.get_comment().await?, @":hourglass: Trying commit pr-1-sha with merge merge-main-sha1-pr-1-sha-0…");
                let repo = tester.default_repo();
                repo.lock()
                    .get_pr(default_pr_number())
                    .check_added_labels(&[]);
                tester.workflow_success(tester.try_branch()).await?;
                tester.expect_comments(1).await;
                let pr = repo.lock().get_pr(default_pr_number()).clone();
                pr.check_added_labels(&["foo", "bar"]);
                pr.check_removed_labels(&["baz"]);
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn try_build_failed_modify_labels(pool: sqlx::PgPool) {
        let world = World::default();
        world.default_repo().lock().set_config(
            r#"
[labels]
try_failed = ["+foo", "+bar", "-baz"]
"#,
        );
        BorsBuilder::new(pool)
            .world(world)
            .run_test(|mut tester| async {
                tester.create_branch(TRY_BRANCH_NAME).expect_suites(1);
                tester.post_comment("@bors try").await?;
                insta::assert_snapshot!(tester.get_comment().await?, @":hourglass: Trying commit pr-1-sha with merge merge-main-sha1-pr-1-sha-0…");
                let repo = tester.default_repo();
                repo.lock()
                    .get_pr(default_pr_number())
                    .check_added_labels(&[]);
                tester.workflow_failure(tester.try_branch()).await?;
                tester.expect_comments(1).await;
                let pr = repo.lock().get_pr(default_pr_number()).clone();
                pr.check_added_labels(&["foo", "bar"]);
                pr.check_removed_labels(&["baz"]);
                Ok(tester)
            })
            .await;
    }
}
