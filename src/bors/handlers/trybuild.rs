use anyhow::anyhow;

use crate::bors::RepositoryClient;
use crate::bors::RepositoryState;
use crate::database::{BuildStatus, DbClient};
use crate::github::{GithubUser, MergeError, PullRequest};
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
    database: &mut dyn DbClient,
    pr: &PullRequest,
    author: &GithubUser,
) -> anyhow::Result<()> {
    log::debug!(
        "Executing try on {}/{}",
        repo.client.repository(),
        pr.number
    );

    if !check_try_permissions(repo, pr, author).await? {
        return Ok(());
    }

    let pr_model = database
        .get_or_create_pull_request(repo.client.repository(), pr.number.into())
        .await?;

    if let Some(ref build) = pr_model.try_build {
        if build.status == BuildStatus::Pending {
            repo.client
                .post_comment(
                    pr.number.into(),
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
            repo.client
                .set_branch_to_sha(TRY_BRANCH_NAME, &merge_sha)
                .await
                .map_err(|error| anyhow!("Cannot set try branch to main branch: {error:?}"))?;

            database
                .attach_try_build(pr_model, TRY_BRANCH_NAME.to_string(), merge_sha.clone())
                .await?;

            repo.client
                .post_comment(
                    pr.number.into(),
                    &format!(
                        ":hourglass: Trying commit {} with merge {merge_sha}…",
                        pr.head.sha
                    ),
                )
                .await?;
        }
        Err(MergeError::Conflict) => {
            repo.client
                .post_comment(pr.number.into(), &merge_conflict_message(&pr.head.name))
                .await?;
        }
        Err(error) => return Err(error.into()),
    }

    Ok(())
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
        repo.client
            .post_comment(
                pr.number.into(),
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
    use crate::bors::handlers::trybuild::TRY_BRANCH_NAME;
    use crate::database::DbClient;
    use crate::github::{CommitSha, MergeError};
    use crate::tests::event::default_pr_number;
    use crate::tests::github::{BranchBuilder, PRBuilder};
    use crate::tests::permissions::NoPermissions;
    use crate::tests::state::{default_merge_sha, default_repo_name, ClientBuilder};

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

        assert_eq!(state.client().get_comment(default_pr_number(), 1), ":exclamation: A try build is currently in progress. You can cancel it using @bors try cancel.");
    }
}
