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
