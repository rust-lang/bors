use crate::PgDbClient;
use crate::bors::gitops_queue::{
    GitOpsCommand, GitOpsQueueSender, PullRequestId, PushCallback, PushCommand,
};
use crate::bors::handlers::{PullRequestData, unapprove_pr};
use crate::bors::{CommandPrefix, Comment, RepositoryState, bors_commit_author};
use crate::database::BuildStatus;
use crate::github::api::CommitAuthor;
use crate::github::api::operations::Commit;
use crate::github::{GithubRepoName, GithubUser};
use crate::permissions::PermissionType;
use std::fmt::Write;
use std::sync::Arc;

/// Entry point for the squash command.
/// This function validates the command and enqueues the actual work to the gitops queue.
pub(super) async fn command_squash(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
    bot_prefix: &CommandPrefix,
    gitops_queue: &GitOpsQueueSender,
) -> anyhow::Result<()> {
    let send_comment = async |text: String| {
        repo_state
            .client
            .post_comment(pr.number(), Comment::new(text), &db)
            .await?;
        anyhow::Ok(())
    };

    #[cfg(not(test))]
    {
        if author.username.to_lowercase() != "kobzol" {
            send_comment(
                ":key: Squashing is currently experimental and cannot be used yet.".to_string(),
            )
            .await?;
            return Ok(());
        }
    }

    let is_reviewer = repo_state
        .permissions
        .load()
        .has_permission(author.id, PermissionType::Review);
    let is_author = author.id == pr.github.author.id;

    if !is_author && !is_reviewer {
        send_comment(":key: Only the PR author or reviewers can squash commits.".to_string())
            .await?;
        return Ok(());
    }

    if !pr.github.editable_by_maintainers {
        send_comment(":key: The `Allow edits by maintainers` option is not enabled on this PR. It is required for squashing to work.".to_string()).await?;
        return Ok(());
    }

    let fork_error =
        || ":fork_and_knife: The squash command can only be used on PRs from a fork.".to_string();
    let Some(fork_repository) = pr
        .github
        .head_repository
        .clone()
        .take_if(|repo| validate_fork(&pr.github.author, repo_state.repository(), repo))
    else {
        send_comment(fork_error()).await?;
        return Ok(());
    };

    let pr_model = pr.db;
    if pr_model
        .auto_build
        .as_ref()
        .map(|build| build.status == BuildStatus::Pending)
        .unwrap_or(false)
    {
        send_comment(
            format!(":exclamation: Cannot squash a PR that is currently being tested. Unapprove the PR first using `{bot_prefix} r-`."),
        )
        .await?;
        return Ok(());
    }

    if pr.github.commit_count > 250 {
        send_comment(format!(
            ":exclamation: The PR has too many commits ({}). At most 250 commits can be squashed.",
            pr.github.commit_count
        ))
        .await?;
        return Ok(());
    }

    let pr_id = PullRequestId {
        repo: repo_state.repository().clone(),
        pr: pr.number(),
    };
    if gitops_queue.is_pending(&pr_id) {
        send_comment(":hourglass: This PR already has a pending git operation in progress, please wait until it is completed.".to_string()).await?;
        return Ok(());
    }

    let commits = repo_state
        .client
        .get_pull_request_commits(pr.number())
        .await?;
    if commits.len() < 2 {
        send_comment(":exclamation: The PR has only one commit.".to_string()).await?;
        return Ok(());
    }

    // Extract the first and last commits
    let first_commit = &commits[0];
    let last_commit = commits.last().unwrap();
    let commit_author = last_commit.author.clone().unwrap_or_else(|| CommitAuthor {
        name: pr.github.author.username.clone(),
        email: pr
            .github
            .author
            .email
            .clone()
            .unwrap_or_else(|| bors_commit_author().email),
    });

    // Create the squashed commit on the source repository.
    // We take the parents of the first commit, and the tree of the last commit, to create the
    // squashed commit.
    let commit_msg =
        commit_message.unwrap_or_else(|| generate_squashed_commit_msg(&pr.github.title, &commits));
    let commit = match repo_state
        .client
        .create_commit(
            &last_commit.tree,
            &first_commit.parents,
            &commit_msg,
            &commit_author,
        )
        .await
    {
        Ok(commit) => commit,
        Err(error) => {
            send_comment(format!(
                ":exclamation: Failed to create squashed commit: {error}"
            ))
            .await?;
            return Ok(());
        }
    };

    let target_branch = pr.github.head.name.clone();
    let pr_number = pr.number();

    let fork_repository2 = fork_repository.clone();
    let target_branch2 = target_branch.clone();
    let repo_state2 = repo_state.clone();
    let db2 = db.clone();
    let commit2 = commit.clone();
    let pr_github = pr.github.clone();

    // We have to help inference here by annotating the variable
    let on_finish: PushCallback = Box::new(move |push_result: anyhow::Result<()>| {
        let repo_state = repo_state2;
        let db = db2;
        let fork_repository = fork_repository2;
        let target_branch = target_branch2;
        let commit = commit2;

        Box::pin(async move {
            // TODO: implement git mock for tests? :)
            #[cfg(test)]
            let push_result = {
                push_result?;
                repo_state
                    .client
                    .set_branch_to_sha(
                        &target_branch,
                        &commit,
                        crate::github::api::operations::ForcePush::Yes,
                    )
                    .await
            };

            if let Err(error) = push_result {
                tracing::error!("Push failed: {error:?}",);
                repo_state.client.post_comment(pr_number, Comment::new(format!(
                        ":exclamation: Failed to push the squashed commit to `{fork_repository}:{target_branch}`: {error}"
                    )), &db)
                        .await?;
                return Ok(());
            }

            // Reload the PR from DB, to update approval status
            let Some(pr_model) = db
                .get_pull_request(repo_state.repository(), pr_number)
                .await?
            else {
                return Ok(());
            };
            let unapproved = if pr_model.is_approved() {
                unapprove_pr(&repo_state, &db, &pr_model, &pr_github).await?;
                true
            } else {
                false
            };

            let mut msg = format!(
                ":hammer: {} commits were squashed into {commit}.",
                pr_github.commit_count,
            );
            if unapproved {
                writeln!(msg, "\n\nThe pull request was unapproved.").unwrap();
            }
            repo_state
                .client
                .post_comment(pr_number, Comment::new(msg), &db)
                .await?;
            Ok(())
        })
    });

    // Request a token that will last at least a few minutes, as there might be some preceding
    // operations in the queue.
    let token = repo_state
        .client
        .client()
        .installation_token_with_buffer(chrono::Duration::minutes(5))
        .await?;

    // Enqueue pushing the squashed commit to the fork.
    // We need to use local git operations for this, because GitHub doesn't currently provide an
    // API that would allow us to transfer a commit between two repositories without opening a PR.
    let command = GitOpsCommand::Push(PushCommand {
        source_repo: repo_state.repository().clone(),
        target_repo: fork_repository,
        target_branch,
        commit,
        token,
        on_finish,
    });

    if !gitops_queue.try_send(pr_id, command)? {
        send_comment(
            ":hourglass: There are too many git operations in progress at the moment. Please try again a few minutes later."
                .to_string(),
        )
        .await?;
        return Ok(());
    }
    Ok(())
}

fn generate_squashed_commit_msg(pr_title: &str, commits: &[Commit]) -> String {
    let mut msg = format!("{pr_title}\n\n");
    for commit in commits {
        writeln!(msg, "* {}", commit.message).unwrap();
    }
    msg
}

/// Validate if the given fork is safe for pushing.
fn validate_fork(
    pr_author: &GithubUser,
    source_repo: &GithubRepoName,
    fork: &GithubRepoName,
) -> bool {
    let owner = source_repo.owner().to_lowercase();
    let fork_owner = fork.owner().to_lowercase();

    // PR cannot be from the same organisation.
    // This also excludes the "fork" being the source repository.
    if owner == fork_owner {
        return false;
    }
    // The fork has to be owned by the PR author
    if fork_owner != pr_author.username.to_lowercase() {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use crate::github::GithubRepoName;
    use crate::tests::{
        BorsTester, Comment, Commit, GitHub, PullRequest, Repo, User, default_repo_name, run_test,
    };

    #[sqlx::test]
    async fn squash_non_author_non_reviewer(pool: sqlx::PgPool) {
        run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            ctx.post_comment(Comment::new((), "@bors squash").with_author(User::try_user()))
                .await?;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":key: Only the PR author or reviewers can squash commits."
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn squash_maintainers_can_edit_disabled(pool: sqlx::PgPool) {
        run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            ctx.modify_pr_in_gh((), |pr| pr.maintainers_can_modify = false);
            ctx.post_comment("@bors squash").await?;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":key: The `Allow edits by maintainers` option is not enabled on this PR. It is required for squashing to work."
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn squash_non_fork_pr(pool: sqlx::PgPool) {
        run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            ctx.modify_pr_in_gh((), |pr| pr.head_repository = None);
            ctx.post_comment(Comment::new((), "@bors squash")).await?;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":fork_and_knife: The squash command can only be used on PRs from a fork."
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn squash_tested_pr(pool: sqlx::PgPool) {
        run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            ctx.post_comment("@bors squash").await?;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":exclamation: Cannot squash a PR that is currently being tested. Unapprove the PR first using `@bors r-`."
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn squash_too_many_commits(pool: sqlx::PgPool) {
        run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            ctx.modify_pr_in_gh((), |pr| {
                let commits = (0..250).map(|num| Commit::new(&format!("sha-{num}"), &num.to_string())).collect();
                pr.add_commits(commits);
            });
            ctx.post_comment("@bors squash").await?;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":exclamation: The PR has too many commits (251). At most 250 commits can be squashed."
            );
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn squash_single_commit(pool: sqlx::PgPool) {
        run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            ctx.post_comment("@bors squash").await?;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":exclamation: The PR has only one commit."
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn squash_two_commits(pool: sqlx::PgPool) {
        let gh = run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            ctx.modify_pr_in_gh((), |pr| {
                pr.title = "Foobar".to_string();
                pr.reset_to_single_commit(Commit::from_sha("sha1"));
                pr.add_commits(vec![Commit::from_sha("sha2")]);
            });
            ctx.post_comment("@bors squash").await?;
            ctx.run_gitop_queue().await?;
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

            Ok(())
        })
        .await;
        insta::assert_snapshot!(gh.get_sha_history((), "pr/1"), @"
        pr-1-sha
        sha1
        sha2
        sha2-reauthored-to-git-user
        ");
    }

    #[sqlx::test]
    async fn squash_reviewer(pool: sqlx::PgPool) {
        run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            ctx.modify_pr_in_gh((), |pr| pr.add_commits(vec![Commit::from_sha("foo")]));
            ctx.post_comment(Comment::new((), "@bors squash").with_author(User::reviewer()))
                .await?;
            ctx.run_gitop_queue().await?;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":hammer: 2 commits were squashed into foo-reauthored-to-git-user."
            );

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn squash_pending_git_op_same_pr(pool: sqlx::PgPool) {
        run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            ctx.modify_pr_in_gh((), |pr| pr.add_commits(vec![Commit::from_sha("foo")]));

            ctx.post_comment("@bors squash").await?;
            ctx.post_comment("@bors squash").await?;

            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":hourglass: This PR already has a pending git operation in progress, please wait until it is completed."
            );

            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn squash_twice(pool: sqlx::PgPool) {
        let gh = run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            ctx.modify_pr_in_gh((), |pr| pr.add_commits(vec![Commit::from_sha("foo")]));

            ctx.post_comment("@bors squash").await?;
            ctx.run_gitop_queue().await?;
            ctx.expect_comments((), 1).await;
            let branch = ctx.pr(()).await.get_gh_pr().head_branch_copy();
            assert_eq!(branch.get_commits().len(), 1);

            ctx.modify_pr_in_gh((), |pr| pr.add_commits(vec![Commit::from_sha("bar")]));
            ctx.post_comment("@bors squash").await?;
            ctx.run_gitop_queue().await?;
            ctx.expect_comments((), 1).await;
            let branch = ctx.pr(()).await.get_gh_pr().head_branch_copy();
            assert_eq!(branch.get_commits().len(), 1);

            Ok(())
        })
        .await;
        insta::assert_snapshot!(gh.get_sha_history((), "pr/1"), @"
        pr-1-sha
        foo
        foo-reauthored-to-git-user
        bar
        bar-reauthored-to-git-user
        ");
    }

    #[sqlx::test]
    async fn squash_queue_full(pool: sqlx::PgPool) {
        run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            let pr2 = open_fork_pr(ctx).await?;
            let pr3 = open_fork_pr(ctx).await?;
            let pr4 = open_fork_pr(ctx).await?;
            ctx.modify_pr_in_gh((), |pr| pr.add_commits(vec![Commit::from_sha("pr1 commit")]));

            ctx.post_comment("@bors squash").await?;
            ctx.post_comment(Comment::new(pr2.id(), "@bors squash")).await?;
            ctx.post_comment(Comment::new(pr3.id(), "@bors squash")).await?;
            ctx.post_comment(Comment::new(pr4.id(), "@bors squash")).await?;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(pr4.id()).await?,
                @":hourglass: There are too many git operations in progress at the moment. Please try again a few minutes later."
            );

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn squash_unapprove(pool: sqlx::PgPool) {
        run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            ctx.modify_pr_in_gh((), |pr| {
                pr.add_commits(vec![Commit::from_sha("sha2")]);
            });
            ctx.approve(()).await?;
            ctx.post_comment("@bors squash").await?;
            ctx.run_gitop_queue().await?;
            ctx.expect_comments((), 1).await;
            ctx.pr(()).await.expect_unapproved();

            Ok(())
        })
        .await;
    }

    fn squash_state() -> GitHub {
        let gh = GitHub::default();
        let pr_author = User::default_pr_author();

        // Create fork
        let fork_repo = fork_repo();
        let mut repo = Repo::new(pr_author.clone(), fork_repo.name());
        repo.fork = true;

        // Set the default PR to be from the fork
        gh.default_repo().lock().get_pr_mut(1).head_repository = Some(repo.full_name());
        gh.with_repo(repo)
    }

    fn fork_repo() -> GithubRepoName {
        GithubRepoName::new(&User::default_pr_author().name, default_repo_name().name())
    }

    async fn open_fork_pr(ctx: &mut BorsTester) -> anyhow::Result<PullRequest> {
        ctx.open_pr((), |pr| {
            pr.add_commits(vec![Commit::from_sha("foo")]);
            pr.head_repository = Some(fork_repo());
        })
        .await
    }
}
