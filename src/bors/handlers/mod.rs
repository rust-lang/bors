use anyhow::Context;
use std::sync::Arc;
use tracing::Instrument;

use crate::bors::command::BorsCommand;
use crate::bors::comments::CommandParseErrorComment;
use crate::bors::event::{BorsEvent, PullRequestComment};
use crate::bors::handlers::help::command_help;
use crate::bors::handlers::ping::command_ping;
use crate::bors::handlers::refresh::refresh_repository;
use crate::bors::handlers::trybuild::{command_try_build, command_try_cancel, TRY_BRANCH_NAME};
use crate::bors::handlers::workflow::{
    handle_check_suite_completed, handle_workflow_completed, handle_workflow_started,
};
use crate::bors::{BorsContext, BorsState, RepositoryClient, RepositoryState};
use crate::database::DbClient;
use crate::github::GithubRepoName;
use crate::utils::logging::LogError;

use super::comments::CommandExecuteErrorComment;

mod help;
mod labels;
mod ping;
mod refresh;
mod trybuild;
mod workflow;

/// This function executes a single BORS event, it is the main execution function of the bot.
pub async fn handle_bors_event<Client: RepositoryClient>(
    event: BorsEvent,
    state: Arc<dyn BorsState<Client>>,
    ctx: Arc<BorsContext>,
) -> anyhow::Result<()> {
    let db = Arc::clone(&ctx.db);
    match event {
        BorsEvent::Comment(comment) => {
            // We want to ignore comments made by this bot
            if state.is_comment_internal(&comment) {
                tracing::trace!("Ignoring comment {comment:?} because it was authored by this bot");
                return Ok(());
            }

            if let Some(repo) = get_repo_state(state, &comment.repository) {
                let span = tracing::info_span!(
                    "Comment",
                    pr = format!("{}#{}", comment.repository, comment.pr_number),
                    author = comment.author.username
                );
                let pr_number = comment.pr_number;
                if let Err(error) = handle_comment(Arc::clone(&repo), db, ctx, comment)
                    .instrument(span.clone())
                    .await
                {
                    span.log_error(error);
                    repo.client
                        .post_comment(pr_number, Box::new(CommandExecuteErrorComment))
                        .await
                        .context("Cannot send comment reacting to an error")?;
                }
            }
        }
        BorsEvent::InstallationsChanged => {
            let span = tracing::info_span!("Repository reload");
            if let Err(error) = state.reload_repositories().instrument(span.clone()).await {
                span.log_error(error);
            }
        }
        BorsEvent::WorkflowStarted(payload) => {
            if let Some(_) = get_repo_state(state, &payload.repository) {
                let span = tracing::info_span!(
                    "Workflow started",
                    repo = payload.repository.to_string(),
                    id = payload.run_id.into_inner()
                );
                if let Err(error) = handle_workflow_started(db, payload)
                    .instrument(span.clone())
                    .await
                {
                    span.log_error(error);
                }
            }
        }
        BorsEvent::WorkflowCompleted(payload) => {
            if let Some(repo) = get_repo_state(state, &payload.repository) {
                let span = tracing::info_span!(
                    "Workflow completed",
                    repo = payload.repository.to_string(),
                    id = payload.run_id.into_inner()
                );
                if let Err(error) = handle_workflow_completed(repo, db, payload)
                    .instrument(span.clone())
                    .await
                {
                    span.log_error(error);
                }
            }
        }
        BorsEvent::CheckSuiteCompleted(payload) => {
            if let Some(repo) = get_repo_state(state, &payload.repository) {
                let span = tracing::info_span!(
                    "Check suite completed",
                    repo = payload.repository.to_string(),
                );
                if let Err(error) = handle_check_suite_completed(repo, db, payload)
                    .instrument(span.clone())
                    .await
                {
                    span.log_error(error);
                }
            }
        }
        BorsEvent::Refresh => {
            let span = tracing::info_span!("Refresh");
            let repos = state.get_all_repos();
            futures::future::join_all(repos.into_iter().map(|repo| {
                let repo = Arc::clone(&repo);
                async {
                    let subspan = tracing::info_span!("Repo", repo = repo.repository.to_string());
                    refresh_repository(repo, Arc::clone(&db))
                        .instrument(subspan)
                        .await
                }
            }))
            .instrument(span)
            .await;
        }
    }
    Ok(())
}

fn get_repo_state<Client: RepositoryClient>(
    state: Arc<dyn BorsState<Client>>,
    repo: &GithubRepoName,
) -> Option<Arc<RepositoryState<Client>>> {
    match state.get_repo_state(repo) {
        Some(result) => Some(result),
        None => {
            tracing::warn!("Repository {} not found", repo);
            None
        }
    }
}

async fn handle_comment<Client: RepositoryClient>(
    repo: Arc<RepositoryState<Client>>,
    database: Arc<dyn DbClient>,
    ctx: Arc<BorsContext>,
    comment: PullRequestComment,
) -> anyhow::Result<()> {
    let pr_number = comment.pr_number;
    let commands = ctx.parser.parse_commands(&comment.text);

    tracing::debug!("Commands: {commands:?}");
    tracing::trace!("Text: {}", comment.text);

    // When we can't load the PR from github, just end the comment handling.
    // We should not return an error to avoid the top level handler to post a comment that
    // an error has occured.
    // FIXME: retry the call if it fails
    let pull_request = match repo.client.get_pull_request(pr_number).await {
        Ok(pr) => pr,
        Err(error) => {
            tracing::error!("Cannot get PR information: {error:?}");
            return Ok(());
        }
    };

    for command in commands {
        match command {
            Ok(command) => {
                let repo = Arc::clone(&repo);
                let database = Arc::clone(&database);
                let result = match command {
                    BorsCommand::Help => {
                        let span = tracing::info_span!("Help");
                        command_help(repo, &pull_request).instrument(span).await
                    }
                    BorsCommand::Ping => {
                        let span = tracing::info_span!("Ping");
                        command_ping(repo, &pull_request).instrument(span).await
                    }
                    BorsCommand::Try { parent } => {
                        let span = tracing::info_span!("Try");
                        command_try_build(repo, database, &pull_request, &comment.author, parent)
                            .instrument(span)
                            .await
                    }
                    BorsCommand::TryCancel => {
                        let span = tracing::info_span!("Cancel try");
                        command_try_cancel(repo, database, &pull_request, &comment.author)
                            .instrument(span)
                            .await
                    }
                };
                if result.is_err() {
                    return result.context("Cannot execute Bors command");
                }
            }
            Err(error) => {
                let comment = CommandParseErrorComment::from(error);
                tracing::warn!("{}", comment);
                repo.client
                    .post_comment(pull_request.number, Box::new(comment))
                    .await
                    .context("Could not reply to PR comment")?;
            }
        }
    }
    Ok(())
}

/// Is this branch interesting for the bot?
fn is_bors_observed_branch(branch: &str) -> bool {
    branch == TRY_BRANCH_NAME
}

#[cfg(test)]
mod tests {
    use crate::tests::event::{comment, default_pr_number};
    use crate::tests::state::{test_bot_user, ClientBuilder};

    #[tokio::test]
    async fn test_ignore_bot_comment() {
        let state = ClientBuilder::default().create_state().await;
        state
            .comment(comment("@bors ping").author(test_bot_user()).create())
            .await;
        state.client().check_comments(default_pr_number(), &[]);
    }

    #[tokio::test]
    async fn test_do_not_comment_when_pr_fetch_fails() {
        let state = ClientBuilder::default().create_state().await;
        state
            .client()
            .set_get_pr_fn(|_| Err(anyhow::anyhow!("Foo")));
        state.comment(comment("foo").create()).await;
        state.client().check_comments(default_pr_number(), &[]);
    }
}
