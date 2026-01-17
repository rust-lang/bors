use crate::PgDbClient;
use crate::bors::handlers::{PullRequestData, unapprove_pr};
use crate::bors::{CommandPrefix, Comment, RepositoryState, bors_commit_author};
use crate::database::BuildStatus;
use crate::github::api::client::CommitAuthor;
use crate::github::{CommitSha, GithubRepoName, GithubUser};
use crate::permissions::PermissionType;
use anyhow::Context;
use secrecy::SecretString;
use std::fmt::Write;
use std::sync::Arc;

pub(super) async fn command_squash(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
    bot_prefix: &CommandPrefix,
) -> anyhow::Result<()> {
    let send_comment = async |text: String| {
        repo_state
            .client
            .post_comment(pr.number(), Comment::new(text), &db)
            .await?;
        anyhow::Ok(())
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
    let fork_repository = match pr.github.head_repository.clone() {
        None => {
            send_comment(fork_error()).await?;
            return Ok(());
        }
        Some(repo) if repo == *repo_state.repository() => {
            send_comment(fork_error()).await?;
            return Ok(());
        }
        Some(repo) => repo,
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
    let author = last_commit.author.clone().unwrap_or_else(|| CommitAuthor {
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
    let commit_msg = &pr.github.title;
    let commit = match repo_state
        .client
        .create_commit(
            &last_commit.tree,
            &first_commit.parents,
            commit_msg,
            &author,
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

    // Push the squashed commit to the fork.
    // We need to use local git operations for this, because GitHub doesn't currently provide an
    // API that would allow us to transfer a commit between two repositories without opening a PR.
    #[cfg(not(test))]
    let result = {
        use tracing::Instrument;

        let token = repo_state.client.client().installation_token().await?;
        let source_repo = repo_state.repository().clone();
        let commit_sha = commit.clone();
        let fork = fork_repository.clone();
        let fork_branch_name = pr.github.head.name.clone();
        let span = tracing::debug_span!(
            "push commit to fork",
            "{source_repo}:{commit_sha} -> {fork}:{fork_branch_name}"
        );
        tokio::task::spawn_blocking(move || {
            push_to_fork(&source_repo, &fork, &commit_sha, &fork_branch_name, token)
        })
        .instrument(span)
        .await?
    };

    // TODO: implement git mock for tests? :)
    #[cfg(test)]
    let result = repo_state
        .client
        .set_branch_to_sha(
            &pr.github.head.name,
            &commit,
            crate::github::api::operations::ForcePush::Yes,
        )
        .await;

    match result {
        Ok(_) => {}
        Err(error) => {
            send_comment(format!(
                ":exclamation: Failed to push the squashed commit to `{fork_repository}`: {error:?}"
            ))
            .await?;
            return Ok(());
        }
    }

    let unapproved = if pr_model.is_approved() {
        unapprove_pr(&repo_state, &db, pr_model, pr.github).await?;
        true
    } else {
        false
    };

    let mut msg = format!(
        ":hammer: {} commits were squashed into {commit}.",
        pr.github.commit_count,
    );
    if unapproved {
        writeln!(msg, "\n\nThe pull request was unapproved.").unwrap();
    }
    send_comment(msg).await
}

#[allow(unused)]
fn push_to_fork(
    source_repo: &GithubRepoName,
    fork_repo: &GithubRepoName,
    commit_sha: &CommitSha,
    branch_name: &str,
    token: SecretString,
) -> anyhow::Result<()> {
    use git2::{Cred, FetchOptions, PushOptions, RemoteCallbacks, Repository};
    use secrecy::ExposeSecret;

    let source_repo_url = format!("https://github.com/{source_repo}.git");
    let target_repo_url = format!("https://github.com/{fork_repo}.git");
    let target_branch = format!("refs/heads/{branch_name}");

    // Create a temporary directory for the local repository
    let temp_dir = tempfile::tempdir()?;
    let repo_path = temp_dir.path();
    let repo = Repository::init_bare(repo_path)?;

    let callbacks = || {
        let token = token.clone();
        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(move |_url, _username_from_url, _allowed_types| {
            Cred::userpass_plaintext("bors", token.expose_secret())
        });
        callbacks
    };

    let mut source_remote = repo.remote_anonymous(&source_repo_url)?;
    let mut fetch_options = FetchOptions::new();
    fetch_options.remote_callbacks(callbacks());

    tracing::debug!("Fetching commit");
    // Fetch the commit from the source repo
    source_remote
        .fetch(&[commit_sha.as_ref()], Some(&mut fetch_options), None)
        .with_context(|| anyhow::anyhow!("Cannot fethc commit {commit_sha} from {source_repo}"))?;

    let oid = git2::Oid::from_str(commit_sha.as_ref())?;
    if let Err(error) = repo.find_commit(oid) {
        return Err(anyhow::anyhow!(
            "Cannot find commit {commit_sha} from {source_repo}: {error:?}"
        ));
    }

    // Create the refspec: push the commit to the target branch
    // The `+` sign says that it is a force push
    let refspec = format!("+{commit_sha}:{target_branch}");

    tracing::debug!("Pushing commit");
    // And push it to the fork repo
    let mut target_remote = repo.remote_anonymous(&target_repo_url)?;
    let mut push_options = PushOptions::new();
    push_options.remote_callbacks(callbacks());

    target_remote
        .push(&[&refspec], Some(&mut push_options))
        .with_context(|| anyhow::anyhow!("Cannot push commit {commit_sha} to {fork_repo}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::github::GithubRepoName;
    use crate::tests::{
        BorsTester, Comment, Commit, GitHub, Repo, User, default_repo_name, run_test,
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
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":hammer: 2 commits were squashed into sha2-reauthored-to-git-user."
            );
            let branch = ctx.pr(()).await.get_gh_pr().head_branch_copy();
            assert_eq!(branch.get_commits().len(), 1);
            insta::assert_debug_snapshot!(branch.get_commit(), @r#"
            Commit {
                sha: "sha2-reauthored-to-git-user",
                message: "Foobar",
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
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":hammer: 2 commits were squashed into foo-reauthored-to-git-user."
            );

            Ok(())
        })
        .await;
    }

    fn squash_state() -> GitHub {
        let gh = GitHub::default();
        let pr_author = User::default_pr_author();

        // Create fork
        let fork_repo = GithubRepoName::new(&pr_author.name, default_repo_name().name());
        let mut repo = Repo::new(pr_author.clone(), fork_repo.name());
        repo.fork = true;

        // Set the default PR to be from the fork
        gh.default_repo().lock().get_pr_mut(1).head_repository = Some(repo.full_name());
        gh.with_repo(repo)
    }
}
