use crate::bors::command::parser::{parse_commands, CommandParseError};
use crate::bors::command::BorsCommand;
use crate::bors::event::{BorsEvent, PullRequestComment};
use crate::bors::handlers::ping::command_ping;
use crate::bors::handlers::trybuild::command_try_build;
use crate::bors::{RepositoryClient, RepositoryState};
use crate::database::DbClient;
use crate::github::GithubAppState;
use anyhow::Context;

mod ping;
mod trybuild;

pub async fn handle_bors_event(
    event: BorsEvent,
    state: &mut GithubAppState,
    database: &mut dyn DbClient,
) -> anyhow::Result<()> {
    match event {
        BorsEvent::Comment(comment) => {
            // We want to ignore comments made by this bot
            if state.is_comment_internal(&comment) {
                log::trace!("Ignoring comment {comment:?} because it was authored by this bot");
                return Ok(());
            }

            match state.get_repository_state_mut(&comment.repository) {
                Some(repo) => {
                    if let Err(error) = handle_comment(repo, comment, database).await {
                        log::warn!("Error occured while handling comment: {error:?}");
                    }
                }
                None => {
                    log::warn!("Repository {} not found", comment.repository);
                }
            }
        }
        BorsEvent::InstallationsChanged => {
            log::info!("Reloading installation repositories");
            if let Err(error) = state.reload_repositories().await {
                log::error!("Could not reload installation repositories: {error:?}");
            }
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
