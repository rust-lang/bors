use std::future::Future;

use anyhow::Context;
use tokio::sync::mpsc;

use crate::command::parser::{parse_commands, CommandParseError};
use crate::command::BorsCommand;
use crate::database::{DbClient, SeaORMClient};
use crate::github::api::{GithubAppState, RepositoryState};
use crate::github::webhook::{PullRequestComment, WebhookEvent, WorkflowFinished};
use crate::handlers::ping::command_ping;
use crate::handlers::trybuild::{command_try_build, handle_workflow_try_build};
use crate::handlers::RepositoryClient;

pub type WebhookSender = mpsc::Sender<WebhookEvent>;

/// Creates a future with a Bors process that receives webhook events and reacts to them.
pub fn create_bors_process(
    mut state: GithubAppState,
    mut database: SeaORMClient,
) -> (WebhookSender, impl Future<Output = ()>) {
    let (tx, mut rx) = mpsc::channel::<WebhookEvent>(1024);

    let service = async move {
        while let Some(event) = rx.recv().await {
            log::trace!("Received webhook: {event:#?}");

            match event {
                WebhookEvent::Comment(comment) => {
                    // We want to ignore comments made by this bot
                    if state.is_comment_internal(&comment) {
                        log::trace!(
                            "Ignoring comment {comment:?} because it was authored by this bot"
                        );
                        continue;
                    }

                    match state.get_repository_state_mut(&comment.repository) {
                        Some(repo) => {
                            if let Err(error) = handle_comment(repo, comment, &mut database).await {
                                log::warn!("Error occured while handling comment: {error:?}");
                            }
                        }
                        None => {
                            log::warn!("Repository {} not found", comment.repository);
                        }
                    }
                }
                WebhookEvent::InstallationsChanged => {
                    log::info!("Reloading installation repositories");
                    if let Err(error) = state.reload_repositories().await {
                        log::error!("Could not reload installation repositories: {error:?}");
                    }
                }
                WebhookEvent::WorkflowFinished(payload) => {
                    if let Err(error) =
                        handle_workflow_event(payload, &mut state, &mut database).await
                    {
                        log::error!(
                            "Error occured while handing workflow finished event: {error:?}"
                        );
                    }
                }
            }
        }
    };
    (tx, service)
}

async fn handle_workflow_event(
    event: WorkflowFinished,
    state: &mut GithubAppState,
    database: &mut dyn DbClient,
) -> anyhow::Result<()> {
    match state.get_repository_state_mut(&event.repository) {
        Some(repo) => {
            if handle_workflow_try_build(&event, repo, database).await? {
                return Ok(());
            }
        }
        None => {
            log::warn!("Repository {} not found", event.repository);
        }
    }
    Ok(())
}

async fn handle_comment<Client: RepositoryClient>(
    repo: &mut RepositoryState<Client>,
    comment: PullRequestComment,
    database: &mut dyn DbClient,
) -> anyhow::Result<()> {
    let pr_number = comment.pr_number;
    let commands = parse_commands(&comment.text);
    let pull_request = repo.client.get_pull_request(pr_number.into()).await?;

    log::info!(
        "Received comment at https://github.com/{}/{}/issues/{}, commands: {:?}",
        repo.repository.owner(),
        repo.repository.name(),
        pr_number,
        commands
    );

    for command in commands {
        match command {
            Ok(command) => {
                let result = match command {
                    BorsCommand::Ping => command_ping(repo, &pull_request).await,
                    BorsCommand::Try => {
                        command_try_build(repo, database, &pull_request, &comment.author).await
                    }
                };
                if result.is_err() {
                    return result.context("Cannot execute Bors command");
                }
            }
            Err(error) => {
                let error_msg = match error {
                    CommandParseError::MissingCommand => "Missing command.".to_string(),
                    CommandParseError::UnknownCommand(command) => {
                        format!(r#"Unknown command "{command}"."#)
                    }
                };

                repo.client
                    .post_comment(pull_request.number.into(), &error_msg)
                    .await
                    .context("Could not reply to PR comment")?;
            }
        }
    }
    Ok(())
}
