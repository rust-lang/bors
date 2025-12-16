use std::sync::Arc;

use super::{PullRequestData, deny_request};
use super::{has_permission, hide_build_started_comments};
use crate::PgDbClient;
use crate::bors::command::{CommandPrefix, Parent};
use crate::bors::comment::try_build_cancelled_comment;
use crate::bors::comment::try_build_cancelled_with_failed_workflow_cancel_comment;
use crate::bors::comment::{CommentTag, no_try_build_in_progress_comment};
use crate::bors::comment::{
    cant_find_last_parent_comment, merge_conflict_comment, try_build_started_comment,
};
use crate::bors::handlers::workflow::{CancelBuildError, cancel_build};
use crate::bors::{MergeType, RepositoryState, TRY_BRANCH_NAME, create_merge_commit_message};
use crate::database::{BuildModel, BuildStatus, PullRequestModel};
use crate::github::api::client::{CheckRunOutput, GithubRepositoryClient};
use crate::github::api::operations::ForcePush;
use crate::github::{CommitSha, GithubUser, PullRequestNumber};
use crate::github::{MergeResult, attempt_merge};
use crate::permissions::PermissionType;
use anyhow::{Context, anyhow};
use octocrab::params::checks::CheckRunConclusion;
use octocrab::params::checks::CheckRunStatus;
use tracing::log;

// This branch serves for preparing the final commit.
// It will be reset to master and merged with the branch that should be tested.
// Because this action (reset + merge) is not atomic, this branch should not run CI checks to avoid
// starting them twice.
pub(super) const TRY_MERGE_BRANCH_NAME: &str = "automation/bors/try-merge";

// The name of the check run seen in the GitHub UI.
pub(super) const TRY_BUILD_CHECK_RUN_NAME: &str = "Bors try build";

/// Performs a so-called try build - merges the PR branch into a special branch designed
/// for running CI checks.
///
/// If `parent` is set, it will use it as a base commit for the merge.
/// Otherwise, it will use the latest commit on the main repository branch.
pub(super) async fn command_try_build(
    repo: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
    parent: Option<Parent>,
    jobs: Vec<String>,
    bot_prefix: &CommandPrefix,
) -> anyhow::Result<()> {
    let repo = repo.as_ref();
    if !has_permission(repo, author, pr, PermissionType::Try).await? {
        deny_request(repo, pr.number(), author, PermissionType::Try).await?;
        return Ok(());
    }

    if Some(Parent::Last) == parent && pr.db.try_build.is_none() {
        tracing::warn!("try build was requested with parent=last but no previous build was found");
        repo.client
            .post_comment(pr.number(), cant_find_last_parent_comment())
            .await?;
        return Ok(());
    };

    let base_sha = match get_base_sha(pr.db, parent) {
        Some(base_sha) => base_sha,
        None => repo
            .client
            .get_branch_sha(&pr.github.base.name)
            .await
            .context(format!("Cannot get SHA for branch {}", pr.github.base.name))?,
    };

    // Try to cancel any previously running try build workflows
    let cancelled_workflow_urls = if let Some(build) = get_pending_build(pr.db) {
        let res = cancel_previous_try_build(repo, &db, build).await?;
        // Also try to hide previous "Try build started" comments that weren't hidden yet
        if let Err(error) =
            hide_build_started_comments(repo, &db, pr.db, CommentTag::TryBuildStarted).await
        {
            tracing::error!("Failed to hide previous try build started comment(s): {error:?}");
        }

        res
    } else {
        vec![]
    };

    match attempt_merge(
        &repo.client,
        TRY_MERGE_BRANCH_NAME,
        &pr.github.head.sha,
        &base_sha,
        &create_merge_commit_message(pr, MergeType::Try { try_jobs: jobs }),
    )
    .await?
    {
        MergeResult::Success(merge_sha) => {
            // If the merge was succesful, run CI with merged commit
            let build_id =
                run_try_build(&repo.client, &db, pr.db, merge_sha.clone(), base_sha).await?;

            // Create a check run to track the try build status in GitHub's UI.
            // This gets added to the PR's head SHA so GitHub shows UI in the checks tab and
            // the bottom of the PR.
            match repo
                .client
                .create_check_run(
                    TRY_BUILD_CHECK_RUN_NAME,
                    &pr.github.head.sha,
                    CheckRunStatus::InProgress,
                    CheckRunOutput {
                        title: "Bors try build".to_string(),
                        summary: "".to_string(),
                    },
                    &build_id.to_string(),
                )
                .await
            {
                Ok(check_run) => {
                    db.update_build_check_run_id(build_id, check_run.id.into_inner() as i64)
                        .await?;
                }
                Err(error) => {
                    // Check runs aren't critical, don't block progress if they fail
                    log::error!("Cannot create check run: {error:?}");
                }
            }

            let comment = repo
                .client
                .post_comment(
                    pr.number(),
                    try_build_started_comment(
                        &pr.github.head.sha,
                        &merge_sha,
                        bot_prefix,
                        cancelled_workflow_urls,
                    ),
                )
                .await?;
            db.record_tagged_bot_comment(
                repo.repository(),
                pr.number(),
                CommentTag::TryBuildStarted,
                &comment.node_id,
            )
            .await?;
        }
        MergeResult::Conflict => {
            repo.client
                .post_comment(pr.number(), merge_conflict_comment(&pr.github.head.name))
                .await?;
        }
    }
    Ok(())
}

/// Cancels a previously running try build and returns a list of cancelled workflow URLs.
async fn cancel_previous_try_build(
    repo: &RepositoryState,
    db: &PgDbClient,
    build: &BuildModel,
) -> anyhow::Result<Vec<String>> {
    assert_eq!(build.status, BuildStatus::Pending);

    match cancel_build(&repo.client, db, build, CheckRunConclusion::Cancelled).await {
        Ok(workflows) => {
            tracing::info!("Try build cancelled");
            Ok(workflows.into_iter().map(|w| w.url).collect())
        }
        Err(CancelBuildError::FailedToMarkBuildAsCancelled(error)) => Err(error),
        Err(CancelBuildError::FailedToCancelWorkflows(error)) => {
            tracing::error!(
                "Could not cancel workflows for SHA {}: {error:?}",
                build.commit_sha
            );
            Ok(vec![])
        }
    }
}

async fn run_try_build(
    client: &GithubRepositoryClient,
    db: &PgDbClient,
    pr: &PullRequestModel,
    commit_sha: CommitSha,
    parent_sha: CommitSha,
) -> anyhow::Result<i32> {
    client
        .set_branch_to_sha(TRY_BRANCH_NAME, &commit_sha, ForcePush::Yes)
        .await
        .map_err(|error| anyhow!("Cannot set try branch to main branch: {error:?}"))?;

    let build_id = db
        .attach_try_build(pr, TRY_BRANCH_NAME.to_string(), commit_sha, parent_sha)
        .await?;

    tracing::info!("Try build started");
    Ok(build_id)
}

fn get_base_sha(pr_model: &PullRequestModel, parent: Option<Parent>) -> Option<CommitSha> {
    let last_parent = pr_model
        .try_build
        .as_ref()
        .map(|build| CommitSha(build.parent.clone()));

    match parent.clone() {
        Some(parent) => match parent {
            Parent::Last => last_parent,
            Parent::CommitSha(parent) => Some(parent),
        },
        None => None,
    }
}

pub(super) async fn command_try_cancel(
    repo: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
) -> anyhow::Result<()> {
    let repo = repo.as_ref();
    if !has_permission(repo, author, pr, PermissionType::Try).await? {
        deny_request(repo, pr.number(), author, PermissionType::Try).await?;
        return Ok(());
    }

    let pr_number: PullRequestNumber = pr.number();
    let Some(build) = get_pending_build(pr.db) else {
        tracing::info!("No try build found when trying to cancel a try build");
        repo.client
            .post_comment(pr_number, no_try_build_in_progress_comment())
            .await?;
        return Ok(());
    };

    match cancel_build(
        &repo.client,
        db.as_ref(),
        build,
        CheckRunConclusion::Cancelled,
    )
    .await
    {
        Ok(workflows) => {
            tracing::info!("Try build cancelled");

            repo.client
                .post_comment(
                    pr_number,
                    try_build_cancelled_comment(workflows.into_iter().map(|w| w.url)),
                )
                .await?
        }
        Err(CancelBuildError::FailedToMarkBuildAsCancelled(error)) => {
            return Err(error);
        }
        Err(CancelBuildError::FailedToCancelWorkflows(error)) => {
            tracing::error!(
                "Could not cancel workflows for try build with SHA {}: {error:?}",
                build.commit_sha
            );
            repo.client
                .post_comment(
                    pr_number,
                    try_build_cancelled_with_failed_workflow_cancel_comment(),
                )
                .await?
        }
    };

    Ok(())
}

fn get_pending_build(pr: &PullRequestModel) -> Option<&BuildModel> {
    pr.try_build
        .as_ref()
        .and_then(|b| (b.status == BuildStatus::Pending).then_some(b))
}

#[cfg(test)]
mod tests {
    use crate::bors::handlers::trybuild::{
        TRY_BRANCH_NAME, TRY_BUILD_CHECK_RUN_NAME, TRY_MERGE_BRANCH_NAME,
    };
    use crate::database::operations::get_all_workflows;
    use crate::database::{BuildStatus, WorkflowStatus};
    use crate::github::CommitSha;
    use crate::github::api::client::HideCommentReason;
    use crate::tests::BorsTester;
    use crate::tests::default_repo_name;
    use crate::tests::{
        BorsBuilder, Comment, GitHub, User, WorkflowEvent, WorkflowJob, WorkflowRunData, run_test,
    };
    use octocrab::models::JobId;
    use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};

    #[sqlx::test]
    async fn try_success(pool: sqlx::PgPool) {
        run_test(pool.clone(), async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;
            tester.workflow_full_success(tester.try_branch().await).await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @r#"
            :sunny: Try build successful ([Workflow1](https://github.com/rust-lang/borstest/actions/runs/1))
            Build commit: merge-0-pr-1 (`merge-0-pr-1`, parent: `main-sha1`)

            <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge-0-pr-1"} -->
            "#
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_failure(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;
            tester.workflow_full_failure(tester.try_branch().await).await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @":broken_heart: Test for merge-0-pr-1 failed: [Workflow1](https://github.com/rust-lang/borstest/actions/runs/1)"
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_failure_job(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;

            let mut workflow = WorkflowRunData::from(tester.try_branch().await);
            workflow.jobs.push(WorkflowJob {
                id: JobId(42),
                status: WorkflowStatus::Failure,
            });
            workflow.jobs.push(WorkflowJob {
                id: JobId(50),
                status: WorkflowStatus::Failure,
            });
            workflow.jobs.push(WorkflowJob {
                id: JobId(51),
                status: WorkflowStatus::Success,
            });

            tester.modify_repo(&default_repo_name(), |repo| repo.update_workflow_run(workflow.clone(), WorkflowStatus::Pending)).await;
            tester.workflow_full_failure(workflow).await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @r"
            :broken_heart: Test for merge-0-pr-1 failed: [Workflow1](https://github.com/rust-lang/borstest/actions/runs/1). Failed jobs:

            - `Job 42` ([web logs](https://github.com/job-logs/42), [enhanced plaintext logs](https://triage.rust-lang.org/gha-logs/rust-lang/borstest/42))
            - `Job 50` ([web logs](https://github.com/job-logs/50), [enhanced plaintext logs](https://triage.rust-lang.org/gha-logs/rust-lang/borstest/50))
            "
            );
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn try_no_permissions(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .post_comment(Comment::from("@bors try").with_author(User::unprivileged()))
                .await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @"@unprivileged-user: :key: Insufficient privileges: not in try users"
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_only_requires_try_permission(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .post_comment(Comment::from("@bors try").with_author(User::try_user()))
                .await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @r"
            :hourglass: Trying commit pr-1-sha with merge merge-0-pr-1…

            To cancel the try build, run the command `@bors try cancel`.
            "
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_merge_comment(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @r"
            :hourglass: Trying commit pr-1-sha with merge merge-0-pr-1…

            To cancel the try build, run the command `@bors try cancel`.
            "
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_commit_message_strip_description(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .edit_pr((), |pr| {
                    pr.description = r"This is a very good PR.

It fixes so many issues, sir."
                        .to_string();
                })
                .await?;

            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;

            insta::assert_snapshot!(tester.get_branch_commit_message(&tester.try_branch().await).await, @r###"
            Auto merge of #1 - pr-1, r=<try>
            Title of PR 1
            "###);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_commit_message_strip_description_keep_try_jobs(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .edit_pr((), |pr| {
                    pr.description = r"This is a very good PR.

try-job: Foo

It fixes so many issues, sir.

try-job: Bar
"
                    .to_string();
                })
                .await?;

            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;

            insta::assert_snapshot!(tester.get_branch_commit_message(&tester.try_branch().await).await, @r###"
            Auto merge of #1 - pr-1, r=<try>
            Title of PR 1

            try-job: Foo
            try-job: Bar
            "###);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_commit_message_overwrite_try_jobs(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .edit_pr((), |pr| {
                    pr.description = r"This is a very good PR.

try-job: Foo
try-job: Bar
"
                    .to_string();
                })
                .await?;

            tester.post_comment("@bors try jobs=Baz,Baz2").await?;
            tester.expect_comments((), 1).await;

            insta::assert_snapshot!(tester.get_branch_commit_message(&tester.try_branch().await).await, @r###"
            Auto merge of #1 - pr-1, r=<try>
            Title of PR 1


            try-job: Baz
            try-job: Baz2
            "###);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_merge_branch_history(pool: sqlx::PgPool) {
        let gh = run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;
            Ok(())
        })
        .await;
        gh.check_sha_history(
            default_repo_name(),
            TRY_MERGE_BRANCH_NAME,
            &["main-sha1", "merge-0-pr-1"],
        );
        gh.check_sha_history(default_repo_name(), TRY_BRANCH_NAME, &["merge-0-pr-1"]);
    }

    #[sqlx::test]
    async fn try_merge_explicit_parent(pool: sqlx::PgPool) {
        let gh = run_test(pool, async |tester: &mut BorsTester| {
            tester
                .post_comment("@bors try parent=ea9c1b050cc8b420c2c211d2177811e564a4dc60")
                .await?;
            tester.expect_comments((), 1).await;
            Ok(())
        })
        .await;
        gh.check_sha_history(
            default_repo_name(),
            TRY_MERGE_BRANCH_NAME,
            &["ea9c1b050cc8b420c2c211d2177811e564a4dc60", "merge-0-pr-1"],
        );
    }

    #[sqlx::test]
    async fn try_merge_last_parent(pool: sqlx::PgPool) {
        let gh = run_test(pool, async |tester: &mut BorsTester| {
            tester
                .post_comment("@bors try parent=ea9c1b050cc8b420c2c211d2177811e564a4dc60")
                .await?;
            tester.expect_comments((), 1).await;
            tester
                .workflow_full_success(tester.try_branch().await)
                .await?;
            tester.expect_comments((), 1).await;
            tester.post_comment("@bors try parent=last").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
            :hourglass: Trying commit pr-1-sha with merge merge-1-pr-1…

            To cancel the try build, run the command `@bors try cancel`.
            ");
            Ok(())
        })
        .await;
        gh.check_sha_history(
            default_repo_name(),
            TRY_MERGE_BRANCH_NAME,
            &[
                "ea9c1b050cc8b420c2c211d2177811e564a4dc60",
                "merge-0-pr-1",
                "ea9c1b050cc8b420c2c211d2177811e564a4dc60",
                "merge-1-pr-1",
            ],
        );
    }

    #[sqlx::test]
    async fn try_merge_last_parent_unknown(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .post_comment("@bors try parent=last")
                .await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @":exclamation: There was no previous build. Please set an explicit parent or remove the `parent=last` argument to use the default parent.");
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn try_merge_conflict(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.modify_branch(TRY_MERGE_BRANCH_NAME, |branch| branch.merge_conflict = true).await;
            tester
                .post_comment("@bors try")
                .await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r###"
            :lock: Merge conflict

            This pull request and the base branch diverged in a way that cannot
             be automatically merged. Please rebase on top of the latest base
             branch, and let the reviewer approve again.

            <details><summary>How do I rebase?</summary>

            Assuming `self` is your fork and `upstream` is this repository,
             you can resolve the conflict following these steps:

            1. `git checkout pr-1` *(switch to your branch)*
            2. `git fetch upstream HEAD` *(retrieve the latest base branch)*
            3. `git rebase upstream/HEAD -p` *(rebase on top of it)*
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
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn try_merge_insert_into_db(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;
            assert!(
                tester
                    .db()
                    .find_build(
                        &default_repo_name(),
                        TRY_BRANCH_NAME,
                        CommitSha(tester.try_branch().await.get_sha().to_string()),
                    )
                    .await?
                    .is_some()
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_cancel_previous_build_no_workflows(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;
            tester.post_comment("@bors try").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
            :hourglass: Trying commit pr-1-sha with merge merge-1-pr-1…

            To cancel the try build, run the command `@bors try cancel`.
            ");
            Ok(())
        })
        .await;
    }

    // Make sure that the second try command knows about the result of the first one.
    #[sqlx::test]
    async fn multiple_commands_in_one_comment(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .post_comment(
                    r#"
@bors try
@bors try cancel
"#,
                )
                .await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
            :hourglass: Trying commit pr-1-sha with merge merge-0-pr-1…

            To cancel the try build, run the command `@bors try cancel`.
            ");
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @"Try build cancelled. Cancelled workflows:");
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_cancel_previous_build_running_workflows(pool: sqlx::PgPool) {
        let gh = run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;
            tester
                .workflow_start(WorkflowRunData::from(tester.try_branch().await).with_run_id(123))
                .await?;

            tester.post_comment("@bors try").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
            :hourglass: Trying commit pr-1-sha with merge merge-1-pr-1…

            (The previously running try build was automatically cancelled.)

            To cancel the try build, run the command `@bors try cancel`.
            ");
            Ok(())
        })
        .await;
        gh.check_cancelled_workflows(default_repo_name(), &[123]);
    }

    #[sqlx::test]
    async fn try_again_after_checks_finish(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;
            tester
                .workflow_full_success(WorkflowRunData::from(tester.try_branch().await).with_run_id(1))
                .await?;
            tester.expect_comments((), 1).await;

            tester.post_comment("@bors try").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
            :hourglass: Trying commit pr-1-sha with merge merge-1-pr-1…

            To cancel the try build, run the command `@bors try cancel`.
            ");
            tester
                .workflow_full_success(
                    WorkflowRunData::from(tester.try_branch().await)
                        .with_run_id(2)
                        .with_check_suite_id(2),
                )
                .await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r#"
            :sunny: Try build successful ([Workflow1](https://github.com/rust-lang/borstest/actions/runs/2))
            Build commit: merge-1-pr-1 (`merge-1-pr-1`, parent: `main-sha1`)

            <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge-1-pr-1"} -->
            "#);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_cancel_no_running_build(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try cancel").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @":exclamation: There is currently no try build in progress.");
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn try_cancel_cancel_workflows(pool: sqlx::PgPool) {
        let gh = run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;

            let branch = tester.try_branch().await;
            tester
                .workflow_event(WorkflowEvent::started(
                    WorkflowRunData::from(branch.clone()).with_run_id(123),
                ))
                .await?;
            tester
                .workflow_event(WorkflowEvent::started(
                    WorkflowRunData::from(branch.clone()).with_run_id(124),
                ))
                .await?;
            tester.post_comment("@bors try cancel").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
            Try build cancelled. Cancelled workflows:
            - https://github.com/rust-lang/borstest/actions/runs/123
            - https://github.com/rust-lang/borstest/actions/runs/124
            ");
            Ok(())
        })
        .await;
        gh.check_cancelled_workflows(default_repo_name(), &[123, 124]);
    }

    #[sqlx::test]
    async fn try_cancel_error(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.modify_repo(&default_repo_name(), |repo| repo.workflow_cancel_error = true).await;
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;
            tester
                .workflow_event(WorkflowEvent::started(tester.try_branch().await))
                .await?;
            tester.post_comment("@bors try cancel").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @"Try build was cancelled. It was not possible to cancel some workflows.");
            let build = tester.db().find_build(
                &default_repo_name(),
                tester.try_branch().await.get_name(),
                tester.try_branch().await.get_sha().to_string().into()
            ).await?.expect("build not found");
            assert_eq!(build.status, BuildStatus::Cancelled);

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_cancel_cancel_in_db(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .modify_repo(&default_repo_name(), |repo| {
                    repo.workflow_cancel_error = true
                })
                .await;
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;
            tester.post_comment("@bors try cancel").await?;
            tester.expect_comments((), 1).await;

            tester.get_pr_copy(()).await.expect_try_build_cancelled();
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_cancel_ignore_finished_workflows(pool: sqlx::PgPool) {
        let gh = run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;
            let branch = tester.try_branch().await;

            let w1 = WorkflowRunData::from(branch.clone()).with_run_id(1);
            let w2 = WorkflowRunData::from(branch.clone()).with_run_id(2);
            let w3 = WorkflowRunData::from(branch).with_run_id(3);
            for workflow in [&w1, &w2, &w3] {
                tester
                    .modify_repo(&default_repo_name(), |repo| {
                        repo.update_workflow_run(workflow.clone(), WorkflowStatus::Pending)
                    })
                    .await;
            }

            tester.workflow_full_success(w1).await?;
            tester.workflow_full_success(w2).await?;
            tester.workflow_start(w3).await?;
            tester.post_comment("@bors try cancel").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
            Try build cancelled. Cancelled workflows:
            - https://github.com/rust-lang/borstest/actions/runs/3
            ");
            Ok(())
        })
        .await;
        gh.check_cancelled_workflows(default_repo_name(), &[3]);
    }

    #[sqlx::test]
    async fn try_workflow_start_after_cancel(pool: sqlx::PgPool) {
        run_test(pool.clone(), async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;
            tester.post_comment("@bors try cancel").await?;
            tester.expect_comments((), 1).await;

            tester
                .workflow_event(WorkflowEvent::started(tester.try_branch().await))
                .await?;
            Ok(())
        })
        .await;
        assert_eq!(get_all_workflows(&pool).await.unwrap().len(), 0);
    }

    #[sqlx::test]
    async fn try_build_failed_modify_labels(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::default().with_default_config(
                r#"
[labels]
try_failed = ["+foo", "+bar", "-baz"]
"#,
            ))
            .run_test(async |tester: &mut BorsTester| {
                tester.post_comment("@bors try").await?;
                insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
                :hourglass: Trying commit pr-1-sha with merge merge-0-pr-1…

                To cancel the try build, run the command `@bors try cancel`.
                ");
                tester.get_pr_copy(()).await.expect_added_labels(&[]);
                tester
                    .workflow_full_failure(tester.try_branch().await)
                    .await?;
                tester.expect_comments((), 1).await;
                tester
                    .get_pr_copy(())
                    .await
                    .expect_added_labels(&["foo", "bar"])
                    .expect_removed_labels(&["baz"]);
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn try_build_creates_check_run(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;

            tester
                .expect_check_run(
                    &tester.get_pr_copy(()).await.get_gh_pr().head_sha,
                    TRY_BUILD_CHECK_RUN_NAME,
                    "Bors try build",
                    CheckRunStatus::InProgress,
                    None,
                )
                .await;

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_build_updates_check_run_on_success(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;

            tester
                .workflow_full_success(tester.try_branch().await)
                .await?;
            tester.expect_comments((), 1).await;

            tester
                .expect_check_run(
                    &tester.get_pr_copy(()).await.get_gh_pr().head_sha,
                    TRY_BUILD_CHECK_RUN_NAME,
                    "Bors try build",
                    CheckRunStatus::Completed,
                    Some(CheckRunConclusion::Success),
                )
                .await;

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_build_updates_check_run_on_failure(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;

            tester
                .workflow_full_failure(tester.try_branch().await)
                .await?;
            tester.expect_comments((), 1).await;

            tester
                .expect_check_run(
                    &tester.get_pr_copy(()).await.get_gh_pr().head_sha,
                    TRY_BUILD_CHECK_RUN_NAME,
                    "Bors try build",
                    CheckRunStatus::Completed,
                    Some(CheckRunConclusion::Failure),
                )
                .await;

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_cancel_updates_check_run_to_cancelled(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;

            tester.post_comment("@bors try cancel").await?;
            tester.expect_comments((), 1).await;

            tester
                .expect_check_run(
                    &tester.get_pr_copy(()).await.get_gh_pr().head_sha,
                    TRY_BUILD_CHECK_RUN_NAME,
                    "Bors try build",
                    CheckRunStatus::Completed,
                    Some(CheckRunConclusion::Cancelled),
                )
                .await;

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn new_try_build_cancels_previous_and_updates_check_run(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;

            let prev_sha = tester.get_pr_copy(()).await.get_gh_pr().head_sha;
            tester.push_to_pr(()).await?;
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;

            tester
                .expect_check_run(
                    &prev_sha,
                    TRY_BUILD_CHECK_RUN_NAME,
                    "Bors try build",
                    CheckRunStatus::Completed,
                    Some(CheckRunConclusion::Cancelled),
                )
                .await;
            tester
                .expect_check_run(
                    &tester.get_pr_copy(()).await.get_gh_pr().head_sha,
                    TRY_BUILD_CHECK_RUN_NAME,
                    "Bors try build",
                    CheckRunStatus::InProgress,
                    None,
                )
                .await;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn hide_try_build_started_comment_after_success(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            let comment = tester.get_next_comment(()).await?;
            tester
                .workflow_full_success(tester.try_branch().await)
                .await?;
            tester.expect_comments((), 1).await;
            tester
                .expect_hidden_comment(&comment, HideCommentReason::Outdated)
                .await;

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn hide_try_build_started_comment_after_failure(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            let comment = tester.get_next_comment(()).await?;
            tester
                .workflow_full_failure(tester.try_branch().await)
                .await?;
            tester.expect_comments((), 1).await;
            tester
                .expect_hidden_comment(&comment, HideCommentReason::Outdated)
                .await;

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn hide_try_build_started_comment_after_restart(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            let comment = tester.get_next_comment(()).await?;

            // Hide the previous "Try build started" comment when we restart the build
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;
            tester
                .expect_hidden_comment(&comment, HideCommentReason::Outdated)
                .await;

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn update_try_build_started_comment_after_workflow_starts(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            let comment = tester.get_next_comment(()).await?;

            tester.workflow_start(tester.try_branch().await).await?;

            // Check that the comment text has been updated with a link to the started workflow
            let updated_comment = tester
                .get_comment_by_node_id(&comment.node_id.unwrap())
                .await
                .unwrap();
            insta::assert_snapshot!(updated_comment.content, @r"
            :hourglass: Trying commit pr-1-sha with merge merge-0-pr-1…

            To cancel the try build, run the command `@bors try cancel`.

            **Workflow**: https://github.com/rust-lang/borstest/actions/runs/1
            ");

            Ok(())
        })
        .await;
    }
}
