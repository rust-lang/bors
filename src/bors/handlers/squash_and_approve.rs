use crate::PgDbClient;
use crate::bors::command::SquashCommitMessage;
use crate::bors::gitops_queue::GitOpsQueueSender;
use crate::bors::handlers::PullRequestData;
use crate::bors::handlers::squash::{SquashResult, command_squash};
use crate::bors::{CommandPrefix, RepositoryState};
use crate::github::GithubUser;
use std::pin::Pin;
use std::sync::Arc;

type SquashCallback =
    dyn Fn(SquashResult) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> + Send;

/// Entry point for the squash command.
/// This function validates the command and enqueues the actual work to the gitops queue.
#[allow(clippy::too_many_arguments)]
pub(super) async fn command_squash_and_approve(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
    commit_message: SquashCommitMessage,
    bot_prefix: &CommandPrefix,
    gitops_queue: &GitOpsQueueSender,
    callback: Box<SquashCallback>,
) -> anyhow::Result<()> {
    let squash_result = command_squash(
        repo_state,
        db,
        pr,
        author,
        commit_message,
        bot_prefix,
        gitops_queue,
    )
    .await?;

    callback(squash_result).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::github::GithubRepoName;
    use crate::tests::default_repo_name;
    use crate::tests::{BorsTester, Commit, GitHub, Repo, User, run_test};
    use std::sync::Arc;

    async fn approve_add_label(pool: sqlx::PgPool) {
        let gh = GitHub::default().append_to_default_config(
            r#"
[labels]
approved = ["+approved"]
"#,
        );
        run_test((pool, gh), async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.pr(()).await.expect_added_labels(&["approved"]);
            Ok(())
        })
        .await;
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn squash_two_commits_and_approve(pool: sqlx::PgPool) {
        let pool = Arc::new(pool);
        let gh = run_test(
            (
                <sqlx::PgPool as Clone>::clone(&*(pool.clone())),
                squash_state(),
            ),
            async |ctx: &mut BorsTester| {
                ctx.modify_pr_in_gh((), |pr| {
                    pr.title = "Foobar".to_string();
                    pr.reset_to_single_commit(Commit::from_sha("sha1"));
                    pr.add_commits(vec![Commit::from_sha("sha2")]);
                });
                ctx.post_comment("@bors squash").await?;
                ctx.run_gitop_queue().await?;
                insta::assert_snapshot!(
                    ctx.get_next_comment_text(()).await?,
                    @":construction: Squashing... this can take a few minutes."
                );
                insta::assert_snapshot!(
                    ctx.get_next_comment_text(()).await?,
                    @":hammer: 2 commits were squashed into sha2-reauthored-to-git-user."
                );
                let branch = ctx.pr(()).await.get_gh_pr().head_branch_copy();
                assert_eq!(branch.get_commits().len(), 1);
                insta::assert_debug_snapshot!(branch.get_commit(), @r#"
            Commit {
                sha: "sha2-reauthored-to-git-user",
                message: "Foobar\n\n* Commit sha1\n* Commit sha2\n",
                author: GitUser {
                    name: "git-user",
                    email: "git-user@git.com",
                },
            }
            "#);
                approve_add_label(<sqlx::PgPool as Clone>::clone(&*(pool.clone()))).await;

                Ok(())
            },
        )
        .await;
        insta::assert_snapshot!(gh.get_sha_history((), "pr/1"), @"
        pr-1-sha
        sha1
        sha2
        sha2-reauthored-to-git-user
        ");
    }

    fn squash_state() -> GitHub {
        let gh = GitHub::default();
        let pr_author = User::default_pr_author();

        // Create fork
        let fork_repo = fork_repo();
        let mut repo = Repo::new(pr_author.clone(), fork_repo.name());
        repo.fork_of = Some(gh.default_repo());

        // Set the default PR to be from the fork
        gh.default_repo().lock().get_pr_mut(1).head_repository = Some(repo.full_name());
        gh.with_repo(repo)
    }
    fn fork_repo() -> GithubRepoName {
        GithubRepoName::new(&User::default_pr_author().name, default_repo_name().name())
    }
}
