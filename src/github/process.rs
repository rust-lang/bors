use std::future::Future;

use anyhow::Context;
use tokio::sync::mpsc;

use crate::command::parser::{parse_commands, CommandParseError};
use crate::command::BorsCommand;
use crate::github::api::{GithubAppState, RepositoryState};
use crate::github::webhook::{PullRequestComment, WebhookEvent};
use crate::github::PullRequest;
use crate::handlers::ping::command_ping;
use crate::handlers::trybuild::command_try_build;
use crate::handlers::RepositoryClient;

pub type WebhookSender = mpsc::Sender<WebhookEvent>;

/// Asynchronous process that receives webhooks and reacts to them.
pub fn github_webhook_process(
    mut state: GithubAppState,
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
                            if let Err(error) = handle_comment(repo, comment).await {
                                log::warn!("Error occured while handling event: {error:?}");
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
            }
        }
    };
    (tx, service)
}

async fn handle_comment(
    repo: &mut RepositoryState,
    comment: PullRequestComment,
) -> anyhow::Result<()> {
    let pr_number = comment.pr_number;
    let commands = parse_commands(&comment.text);
    let pull_request = repo
        .client
        .client
        .pulls(repo.repository.owner(), repo.repository.name())
        .get(pr_number)
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "Could not get PR {}/{pr_number}: {error:?}",
                repo.repository
            )
        })?;

    log::info!(
        "Received comment at https://github.com/{}/{}/issues/{}, commands: {:?}",
        repo.repository.owner(),
        repo.repository.name(),
        pr_number,
        commands
    );

    let pull_request = github_pr_to_pr(pull_request);
    for command in commands {
        match command {
            Ok(command) => {
                let result = match command {
                    BorsCommand::Ping => command_ping(&mut repo.client, &pull_request).await,
                    BorsCommand::Try => {
                        command_try_build(
                            &mut repo.client,
                            &repo.permissions_resolver,
                            &pull_request,
                            &comment.author,
                        )
                        .await
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
                    .post_comment(&pull_request, &error_msg)
                    .await
                    .context("Could not reply to PR comment")?;
            }
        }
    }
    Ok(())
}

fn github_pr_to_pr(pr: octocrab::models::pulls::PullRequest) -> PullRequest {
    PullRequest {
        number: pr.number,
        head_label: pr.head.label.unwrap_or_else(|| "<unknown>".to_string()),
        head_ref: pr.head.ref_field,
        base_ref: pr.base.ref_field,
    }
}
