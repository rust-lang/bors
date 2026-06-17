use crate::PgDbClient;
use crate::bors::comment::CommentTag;
use crate::bors::gitops_queue::{
    GitOpsCommand, GitOpsQueueSender, PullRequestId, PushCallback, PushCommand,
};
use crate::bors::handlers::{
    InvalidationComment, InvalidationInfo, InvalidationReason, PullRequestData, invalidate_pr,
};
use crate::bors::{
    CommandPrefix, Comment, RepositoryState, bors_commit_author, hide_tagged_comments,
};
use crate::database::BuildStatus;
use crate::github::api::CommitAuthor;
use crate::github::api::operations::Commit;
use crate::github::{GithubRepoName, GithubUser};
use crate::permissions::PermissionType;
use std::collections::HashSet;
use std::fmt::Write;
use std::sync::Arc;

/// Entry point for the squash command.
/// This function validates the command and enqueues the actual work to the gitops queue.
pub(super) async fn command_squash(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
    commit_message: Option<String>,
    bot_prefix: &CommandPrefix,
    gitops_queue: &GitOpsQueueSender,
) -> anyhow::Result<()> {
    let send_comment = async |text: String| {
        let comment = repo_state
            .client
            .post_comment(pr.number(), Comment::new(text), &db)
            .await?;
        anyhow::Ok(comment)
    };

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

    let notify_comment = repo_state
        .client
        .post_comment(
            pr.number(),
            Comment::new(":construction: Squashing... this can take a few minutes.".to_string())
                .with_tag(CommentTag::SquashStarted),
            &db,
        )
        .await?;

    // Extract the first and last commits
    let first_commit = &commits[0];
    let last_commit = commits.last().unwrap();

    // It's not really possible to tell who should be the main author.
    // The easiest would be to take the PR author, but we don't know their e-mail through GitHub
    // easily.
    // So we instead take the authorship from the commits.
    // We choose the first commit, as it seems like a reasonable expectation that it will be made
    // by the PR author.
    // Later we also add "Co-authored-by" trailers to add all other authors from the squashed
    // commits.
    let commit_author = first_commit.author.clone().unwrap_or_else(|| CommitAuthor {
        name: pr.github.author.username.clone(),
        email: pr
            .github
            .author
            .email_probably_missing()
            .map(|s| s.to_string())
            .unwrap_or_else(|| bors_commit_author().email),
    });

    // Create the squashed commit on the source repository.
    // We take the parents of the first commit, and the tree of the last commit, to create the
    // squashed commit.
    let commit_msg =
        commit_message.unwrap_or_else(|| generate_squashed_commit_msg(&pr.github.title, &commits));
    let commit_msg = add_coauthored_authors(commit_msg, &commits, &commit_author);
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
            invalidate_pr(
                &repo_state,
                &db,
                &pr_model,
                &pr_github,
                InvalidationInfo::new(InvalidationReason::CommitShaChanged)
                    .with_comment_url(notify_comment.html_url.to_string()),
                Some(
                    InvalidationComment::new(format!(
                        ":hammer: {} commits were squashed into {commit}.",
                        pr_github.commit_count,
                    ))
                    .post_always(),
                ),
            )
            .await?;
            // Hide previous "squash started" comments.
            hide_tagged_comments(&repo_state, &db, &pr_model, CommentTag::SquashStarted).await?;
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

/// Add "Co-authored-by: [name] <[email]>" trailer(s) to the commit message to properly reflect
/// all authors who authored the specified `commits`.
fn add_coauthored_authors(
    mut commit_msg: String,
    commits: &[Commit],
    author: &CommitAuthor,
) -> String {
    // Gather all commit authors.
    // Note that we ignore previous "Co-authored-by" trailers in the original commit messages, to
    // simplify the implementation.
    let mut authors = commits
        .iter()
        .filter_map(|commit| commit.author.clone())
        .collect::<HashSet<CommitAuthor>>();
    // Remove the author of the final commit, because that will be attributed through normal git
    // commit authorship.
    authors.remove(author);

    let mut authors: Vec<_> = authors.into_iter().collect();
    authors.sort();

    if authors.is_empty() {
        return commit_msg;
    }

    while !commit_msg.ends_with("\n\n") {
        commit_msg.push('\n');
    }

    for author in authors {
        writeln!(
            commit_msg,
            "Co-authored-by: {} <{}>",
            author.name, author.email
        )
        .unwrap();
    }

    commit_msg
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
    use crate::github::api::client::HideCommentReason;
    use crate::tests::{
        BorsTester, Comment, Commit, GitHub, GitUser, PullRequest, Repo, User, default_repo_name,
        run_test,
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
                @":construction: Squashing... this can take a few minutes."
            );
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":hammer: 2 commits were squashed into foo-reauthored-to-default-user."
            );
            insta::assert_snapshot!(
                ctx.pr(())
                    .await
                    .get_gh_pr()
                    .head_branch_copy()
                    .get_commit()
                    .author()
                    .name,
                @"default-user"
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
                @":construction: Squashing... this can take a few minutes."
            );
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
            ctx.expect_comments((), 2).await;
            let branch = ctx.pr(()).await.get_gh_pr().head_branch_copy();
            assert_eq!(branch.get_commits().len(), 1);

            ctx.modify_pr_in_gh((), |pr| pr.add_commits(vec![Commit::from_sha("bar")]));
            ctx.post_comment("@bors squash").await?;
            ctx.run_gitop_queue().await?;
            ctx.expect_comments((), 2).await;
            let branch = ctx.pr(()).await.get_gh_pr().head_branch_copy();
            assert_eq!(branch.get_commits().len(), 1);

            Ok(())
        })
        .await;
        insta::assert_snapshot!(gh.get_sha_history((), "pr/1"), @"
        pr-1-sha
        foo
        foo-reauthored-to-default-user
        bar
        bar-reauthored-to-default-user
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
            ctx.expect_comments((), 1).await;
            ctx.expect_comments(pr2.id(), 1).await;
            ctx.expect_comments(pr3.id(), 1).await;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(pr4.id()).await?,
                @":construction: Squashing... this can take a few minutes."
            );
            insta::assert_snapshot!(
                ctx.get_next_comment_text(pr4.id()).await?,
                @":hourglass: There are too many git operations in progress at the moment. Please try again a few minutes later."
            );

            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn squash_hide_squash_started_comment(pool: sqlx::PgPool) {
        run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            ctx.modify_pr_in_gh((), |pr| pr.add_commits(vec![Commit::from_sha("foo")]));
            ctx.post_comment("@bors squash").await?;
            let comment = ctx.get_next_comment(()).await?;
            insta::assert_snapshot!(
                comment.content(),
                @":construction: Squashing... this can take a few minutes."
            );
            ctx.run_gitop_queue().await?;
            ctx.expect_comments((), 1).await;
            ctx.expect_hidden_comment(&comment, HideCommentReason::Outdated);

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
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":construction: Squashing... this can take a few minutes."
            );
            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @"
            :hammer: 2 commits were squashed into sha2-reauthored-to-default-user.

            This pull request was unapproved.
            ");
            ctx.pr(()).await.expect_unapproved();

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn squash_custom_message(pool: sqlx::PgPool) {
        run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            ctx.modify_pr_in_gh((), |pr| {
                pr.add_commits(vec![Commit::from_sha("sha2")]);
            });
            ctx.approve(()).await?;
            ctx.post_comment("@bors squash msg=\"This is a squashed commit\"")
                .await?;
            ctx.run_gitop_queue().await?;
            ctx.expect_comments((), 2).await;
            insta::assert_snapshot!(ctx.pr(()).await.get_gh_pr().head_branch_copy().get_commit().message(), @"
            This is a squashed commit

            Co-authored-by: git-user <git-user@git.com>
            ");

            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn squash_add_coauthored_by(pool: sqlx::PgPool) {
        run_test((pool, squash_state()), async |ctx: &mut BorsTester| {
            ctx.modify_pr_in_gh((), |pr| {
                let pr_author = pr.head_branch_copy().get_commit().author().clone();
                let user1 = GitUser {
                    name: "User 1".to_string(),
                    email: "user1@users.com".to_string(),
                };
                let user2 = GitUser {
                    name: "User 2".to_string(),
                    email: "user2@users.com".to_string(),
                };

                pr.add_commits(vec![
                    Commit::from_sha("sha2").with_author(pr_author.clone()),
                    Commit::from_sha("sha3").with_author(user1.clone()),
                    Commit::from_sha("sha4").with_author(user2),
                    Commit::from_sha("sha5").with_author(user1),
                    Commit::from_sha("sha6").with_author(pr_author)
                ]);
            });
            ctx.approve(()).await?;
            ctx.post_comment("@bors squash")
                .await?;
            ctx.run_gitop_queue().await?;
            ctx.expect_comments((), 2).await;
            insta::assert_snapshot!(ctx.pr(()).await.get_gh_pr().head_branch_copy().get_commit().message(), @"
            Title of PR 1

            * initial PR#1 commit
            * Commit sha2
            * Commit sha3
            * Commit sha4
            * Commit sha5
            * Commit sha6

            Co-authored-by: User 1 <user1@users.com>
            Co-authored-by: User 2 <user2@users.com>
            ");

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
