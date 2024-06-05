use std::sync::Arc;

use anyhow::Context;
use tracing::Instrument;

use crate::bors::command::{BorsCommand, CommandParseError};
use crate::bors::event::{BorsGlobalEvent, BorsRepositoryEvent, PullRequestComment};
use crate::bors::handlers::help::command_help;
use crate::bors::handlers::ping::command_ping;
use crate::bors::handlers::refresh::refresh_repository;
use crate::bors::handlers::trybuild::{command_try_build, command_try_cancel, TRY_BRANCH_NAME};
use crate::bors::handlers::workflow::{
    handle_check_suite_completed, handle_workflow_completed, handle_workflow_started,
};
use crate::bors::{BorsContext, Comment, RepositoryLoader, RepositoryState};
use crate::{PgDbClient, TeamApiClient};

#[cfg(test)]
use crate::tests::util::TestSyncMarker;

mod help;
mod labels;
mod ping;
mod refresh;
mod trybuild;
mod workflow;

#[cfg(test)]
pub static WAIT_FOR_WORKFLOW_STARTED: TestSyncMarker = TestSyncMarker::new();

/// This function executes a single BORS repository event
pub async fn handle_bors_repository_event(
    event: BorsRepositoryEvent,
    ctx: Arc<BorsContext>,
) -> anyhow::Result<()> {
    let db = Arc::clone(&ctx.db);
    let Some(repo) = ctx
        .repositories
        .read()
        .unwrap()
        .get(event.repository())
        .map(Arc::clone)
    else {
        return Err(anyhow::anyhow!(
            "Repository {} not found in the bot state",
            event.repository()
        ));
    };

    match event {
        BorsRepositoryEvent::Comment(comment) => {
            // We want to ignore comments made by this bot
            if repo.client.is_comment_internal(&comment).await? {
                tracing::trace!("Ignoring comment {comment:?} because it was authored by this bot");
                return Ok(());
            }

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
                repo.client
                    .post_comment(
                        pr_number,
                        Comment::new(
                            ":x: Encountered an error while executing command".to_string(),
                        ),
                    )
                    .await
                    .context("Cannot send comment reacting to an error")?;
                return Err(error.context("Cannot perform command"));
            }
        }

        BorsRepositoryEvent::WorkflowStarted(payload) => {
            let span = tracing::info_span!(
                "Workflow started",
                repo = payload.repository.to_string(),
                id = payload.run_id.into_inner()
            );
            handle_workflow_started(db, payload)
                .instrument(span.clone())
                .await?;

            #[cfg(test)]
            WAIT_FOR_WORKFLOW_STARTED.mark();
        }
        BorsRepositoryEvent::WorkflowCompleted(payload) => {
            let span = tracing::info_span!(
                "Workflow completed",
                repo = payload.repository.to_string(),
                id = payload.run_id.into_inner()
            );
            handle_workflow_completed(repo, db, payload)
                .instrument(span.clone())
                .await?;
        }
        BorsRepositoryEvent::CheckSuiteCompleted(payload) => {
            let span = tracing::info_span!(
                "Check suite completed",
                repo = payload.repository.to_string(),
            );
            handle_check_suite_completed(repo, db, payload)
                .instrument(span.clone())
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub static WAIT_FOR_REFRESH: TestSyncMarker = TestSyncMarker::new();

/// This function executes a single BORS global event
pub async fn handle_bors_global_event(
    event: BorsGlobalEvent,
    ctx: Arc<BorsContext>,
    repo_loader: &RepositoryLoader,
    team_api_client: &TeamApiClient,
) -> anyhow::Result<()> {
    let db = Arc::clone(&ctx.db);
    match event {
        BorsGlobalEvent::InstallationsChanged => {
            let span = tracing::info_span!("Installations changed");
            reload_repos(ctx, repo_loader, team_api_client)
                .instrument(span)
                .await?;
        }
        BorsGlobalEvent::Refresh => {
            let span = tracing::info_span!("Refresh");
            let repos: Vec<Arc<RepositoryState>> =
                ctx.repositories.read().unwrap().values().cloned().collect();
            futures::future::join_all(repos.into_iter().map(|repo| {
                let repo = Arc::clone(&repo);
                async {
                    let subspan = tracing::info_span!("Repo", repo = repo.repository.to_string());
                    refresh_repository(repo, Arc::clone(&db), team_api_client)
                        .instrument(subspan)
                        .await
                }
            }))
            .instrument(span)
            .await;

            #[cfg(test)]
            WAIT_FOR_REFRESH.mark();
        }
    }
    Ok(())
}

async fn handle_comment(
    repo: Arc<RepositoryState>,
    database: Arc<PgDbClient>,
    ctx: Arc<BorsContext>,
    comment: PullRequestComment,
) -> anyhow::Result<()> {
    let pr_number = comment.pr_number;
    let commands = ctx.parser.parse_commands(&comment.text);

    tracing::debug!("Commands: {commands:?}");
    tracing::trace!("Text: {}", comment.text);

    let pull_request = repo
        .client
        .get_pull_request(pr_number)
        .await
        .with_context(|| format!("Cannot get information about PR {pr_number}"))?;

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
                    BorsCommand::Try { parent, jobs } => {
                        let span = tracing::info_span!("Try");
                        command_try_build(
                            repo,
                            database,
                            &pull_request,
                            &comment.author,
                            parent,
                            jobs,
                        )
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
                let message = match error {
                    CommandParseError::MissingCommand => "Missing command.".to_string(),
                    CommandParseError::UnknownCommand(command) => {
                        format!(r#"Unknown command "{command}"."#)
                    }
                    CommandParseError::MissingArgValue { arg } => {
                        format!(r#"Unknown value for argument "{arg}"."#)
                    }
                    CommandParseError::UnknownArg(arg) => {
                        format!(r#"Unknown argument "{arg}"."#)
                    }
                    CommandParseError::DuplicateArg(arg) => {
                        format!(r#"Argument "{arg}" found multiple times."#)
                    }
                    CommandParseError::ValidationError(error) => {
                        format!("Invalid command: {error}")
                    }
                };
                tracing::warn!("{}", message);
                repo.client
                    .post_comment(pull_request.number, Comment::new(message))
                    .await
                    .context("Could not reply to PR comment")?;
            }
        }
    }
    Ok(())
}

async fn reload_repos(
    ctx: Arc<BorsContext>,
    repo_loader: &RepositoryLoader,
    team_api_client: &TeamApiClient,
) -> anyhow::Result<()> {
    let reloaded_repos = repo_loader.load_repositories(team_api_client).await?;
    let mut repositories = ctx.repositories.write().unwrap();
    for repo in repositories.values() {
        if !reloaded_repos.contains_key(&repo.repository) {
            tracing::warn!("Repository {} was not reloaded", repo.repository);
        }
    }
    for (name, repo) in reloaded_repos {
        let repo = match repo {
            Ok(repo) => repo,
            Err(error) => {
                tracing::error!("Failed to reload repository {name}: {error:?}");
                continue;
            }
        };

        if repositories.insert(name.clone(), Arc::new(repo)).is_some() {
            tracing::info!("Repository {name} was reloaded");
        } else {
            tracing::info!("Repository {name} was added");
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
    use crate::tests::mocks::{run_test, Comment, User};

    #[sqlx::test]
    async fn ignore_bot_comment(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester
                .post_comment(Comment::from("@bors ping").with_author(User::bors_bot()))
                .await?;
            // Returning here will make sure that no comments were received
            Ok(tester)
        })
        .await;
    }
}
