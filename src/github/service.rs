use std::future::Future;

use anyhow::Context;
use tokio::sync::mpsc;

use crate::command::parser::{parse_commands, CommandParseError};
use crate::command::BorsCommand;
use crate::github::api::{Repository, RepositoryAccess};
use crate::github::webhook::{WebhookContent, WebhookEvent};

pub type WebhookSender = mpsc::Sender<WebhookContent>;

/// Asynchronous process that receives webhooks and reacts to them.
pub fn github_webhook_process(
    access: RepositoryAccess,
) -> (mpsc::Sender<WebhookContent>, impl Future<Output = ()>) {
    let (tx, mut rx) = mpsc::channel::<WebhookContent>(1024);

    let service = async move {
        while let Some(message) = rx.recv().await {
            log::trace!("Received webhook: {message:#?}");

            match access.get_repo(&message.repository) {
                Some(repo) => {
                    if let Err(error) = handle_event(repo, message).await {
                        log::warn!("Error occured while handling event: {error:?}");
                    }
                }
                None => {
                    log::warn!("Repository {} not found", message.repository);
                }
            }
        }
    };
    (tx, service)
}

async fn handle_event(repo: &Repository, message: WebhookContent) -> anyhow::Result<()> {
    match message.event {
        WebhookEvent::Comment(event) => {
            // We only care about pull request comments
            if event.issue.pull_request.is_none() {
                log::trace!(
                    "Ignoring event {event:?} because it does not belong to a pull request"
                );
                return Ok(());
            }
            // We want to ignore comments made by this bot
            if repo.is_comment_internal(&event.comment) {
                log::trace!("Ignoring event {event:?} because it was authored by this bot");
                return Ok(());
            }

            let pr_number = event.issue.number;

            let comment_text = event.comment.body.unwrap_or_default();
            let commands = parse_commands(&comment_text);

            log::info!(
                "Received comment at https://github.com/{}/{}/issues/{}, commands: {:?}",
                repo.repository().owner,
                repo.repository().name,
                pr_number,
                commands
            );
            for command in commands {
                match command {
                    Ok(BorsCommand::Ping) => {
                        execute_bors_command(repo, BorsCommand::Ping, pr_number)
                            .await
                            .context("Could not execute bors command")?
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let error_msg = match error {
                            CommandParseError::MissingCommand => "Missing command.".to_string(),
                            CommandParseError::UnknownCommand(command) => {
                                format!(r#"Unknown command "{command}"."#)
                            }
                        };

                        repo.post_pr_comment(pr_number, &error_msg)
                            .await
                            .context("Could not reply to PR comment")?;
                    }
                }
            }
        }
    }
    Ok(())
}

async fn execute_bors_command(
    repo: &Repository,
    command: BorsCommand,
    pr_number: u64,
) -> anyhow::Result<()> {
    match command {
        BorsCommand::Ping => {
            log::debug!("Executing ping");
            repo.post_pr_comment(pr_number, "Pong 🏓!").await?;
        }
        BorsCommand::Try => {}
    }
    Ok(())
}
