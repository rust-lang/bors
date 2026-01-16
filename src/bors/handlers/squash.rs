use crate::PgDbClient;
use crate::bors::handlers::{PullRequestData, unapprove_pr};
use crate::bors::{CommandPrefix, Comment, RepositoryState, bors_commit_author};
use crate::database::BuildStatus;
use crate::github::api::client::CommitAuthor;
use crate::github::{CommitSha, GithubRepoName, GithubUser};
use anyhow::Context;
use git2::{Cred, FetchOptions, PushOptions, RemoteCallbacks, Repository};
use secrecy::{ExposeSecret, SecretString};
use std::fmt::Write;
use std::sync::Arc;
use tracing::{Instrument, debug_span};

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

    if author.id != pr.github.author.id {
        send_comment(":key: Only the PR author can squash its commits.".to_string()).await?;
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
    let token = repo_state.client.client().installation_token().await?;
    let source_repo = repo_state.repository().clone();
    let commit_sha = commit.clone();
    let fork = fork_repository.clone();
    let fork_branch_name = pr.github.head.name.clone();
    let span = debug_span!(
        "push commit to fork",
        "{source_repo}:{commit_sha} -> {fork}:{fork_branch_name}"
    );
    let fut = tokio::task::spawn_blocking(move || {
        push_to_fork(&source_repo, &fork, &commit_sha, &fork_branch_name, token)
    })
    .instrument(span);
    match fut.await? {
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

fn push_to_fork(
    source_repo: &GithubRepoName,
    fork_repo: &GithubRepoName,
    commit_sha: &CommitSha,
    branch_name: &str,
    token: SecretString,
) -> anyhow::Result<()> {
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
