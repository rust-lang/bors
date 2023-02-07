use std::future::Future;

use anyhow::Context;
use tokio::sync::mpsc;

use crate::command::parser::{parse_commands, CommandParseError};
use crate::command::BorsCommand;
use crate::github::api::{GithubAppClient, RepositoryClient};
use crate::github::event::{PullRequestComment, WebhookEvent};

pub type WebhookSender = mpsc::Sender<WebhookEvent>;

/// Asynchronous process that receives webhooks and reacts to them.
pub fn github_webhook_process(
    mut client: GithubAppClient,
) -> (WebhookSender, impl Future<Output = ()>) {
    let (tx, mut rx) = mpsc::channel::<WebhookEvent>(1024);

    let service = async move {
        while let Some(event) = rx.recv().await {
            log::trace!("Received webhook: {event:#?}");

            match event {
                WebhookEvent::Comment(event) => match client.get_repo_client(&event.repository) {
                    Some(repo) => {
                        if let Err(error) = handle_comment(&client, repo, event).await {
                            log::warn!("Error occured while handling event: {error:?}");
                        }
                    }
                    None => {
                        log::warn!("Repository {} not found", event.repository);
                    }
                },
                WebhookEvent::InstallationsChanged => {
                    log::info!("Reloading installation repositories");
                    if let Err(error) = client.reload_repositories().await {
                        log::error!("Could not reload installation repositories: {error:?}");
                    }
                }
            }
        }
    };
    (tx, service)
}

async fn handle_comment(
    client: &GithubAppClient,
    repo: &RepositoryClient,
    comment: PullRequestComment,
) -> anyhow::Result<()> {
    // We want to ignore comments made by this bot
    if client.is_comment_internal(&comment.user) {
        log::trace!("Ignoring comment {comment:?} because it was authored by this bot");
        return Ok(());
    }

    let pr_number = comment.pr_number;
    let commands = parse_commands(&comment.text);

    log::info!(
        "Received comment at https://github.com/{}/{}/issues/{}, commands: {:?}",
        repo.repository().owner,
        repo.repository().name,
        pr_number,
        commands
    );
    for command in commands {
        match command {
            Ok(BorsCommand::Ping) => execute_bors_command(repo, BorsCommand::Ping, pr_number)
                .await
                .context("Could not execute bors command")?,
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
    Ok(())
}

async fn execute_bors_command(
    repo: &RepositoryClient,
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
