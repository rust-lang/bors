use crate::PgDbClient;
use crate::bors::handlers::{PullRequestData, unapprove_pr};
use crate::bors::{CommandPrefix, Comment, RepositoryState, bors_commit_author};
use crate::database::BuildStatus;
use crate::github::GithubUser;
use crate::github::api::client::CommitAuthor;
use crate::github::api::operations::ForcePush;
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

    if author.id != pr.github.author.id {
        send_comment(":key: Only the PR author can squash its commits.".to_string()).await?;
        return Ok(());
    }

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
    // We take the parents of the first commit, and the tree of the last commit, to create the
    // squashed commit.
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
    let commit_msg = "Squashed commit";
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

    let fork_error =
        || ":fork_and_knife: The squash command can only be used on PRs from a fork.".to_string();
    let fork_repository = match pr.github.head_repository.as_ref() {
        None => {
            send_comment(fork_error()).await?;
            return Ok(());
        }
        Some(repo) if repo == repo_state.repository() => {
            send_comment(fork_error()).await?;
            return Ok(());
        }
        Some(repo) => repo,
    };
    match repo_state
        .client
        .set_fork_branch_to_sha(
            fork_repository,
            &pr.github.head.name,
            &commit,
            ForcePush::YesIfBranchExists,
        )
        .await
    {
        Ok(_) => {}
        Err(error) => {
            send_comment(format!(
                ":exclamation: Failed to push the squashed commit to `{fork_repository}`: {error}"
            ))
            .await?;
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
