use std::sync::Arc;

use crate::bors::command::{BorsCommand, CommandParseError};
use crate::bors::event::{BorsGlobalEvent, BorsRepositoryEvent, PullRequestComment};
use crate::bors::handlers::help::command_help;
use crate::bors::handlers::info::command_info;
use crate::bors::handlers::ping::command_ping;
use crate::bors::handlers::refresh::reload_repository_config;
use crate::bors::handlers::review::{
    command_approve, command_close_tree, command_open_tree, command_unapprove,
};
use crate::bors::handlers::trybuild::{TRY_BRANCH_NAME, command_try_build, command_try_cancel};
use crate::bors::handlers::workflow::{
    handle_check_suite_completed, handle_workflow_completed, handle_workflow_started,
};
use crate::bors::{BorsContext, Comment, RepositoryState};
use crate::database::DelegatedPermission;
use crate::github::{GithubUser, PullRequest};
use crate::permissions::PermissionType;
use crate::{PgDbClient, TeamApiClient, load_repositories};
use anyhow::Context;
use octocrab::Octocrab;
use pr_events::{
    handle_pull_request_closed, handle_pull_request_converted_to_draft, handle_pull_request_edited,
    handle_pull_request_merged, handle_pull_request_opened, handle_pull_request_ready_for_review,
    handle_pull_request_reopened, handle_push_to_branch, handle_push_to_pull_request,
};
use review::{command_delegate, command_set_priority, command_set_rollup, command_undelegate};
use tracing::Instrument;

#[cfg(test)]
use crate::tests::util::TestSyncMarker;

use super::mergeable_queue::MergeableQueueSender;

mod help;
mod info;
mod labels;
mod ping;
mod pr_events;
mod refresh;
mod review;
mod trybuild;
mod workflow;

#[cfg(test)]
pub static WAIT_FOR_WORKFLOW_STARTED: TestSyncMarker = TestSyncMarker::new();

/// This function executes a single BORS repository event
pub async fn handle_bors_repository_event(
    event: BorsRepositoryEvent,
    ctx: Arc<BorsContext>,
    mergeable_queue_tx: MergeableQueueSender,
) -> anyhow::Result<()> {
    let db = Arc::clone(&ctx.db);
    let Some(repo) = ctx
        .repositories
        .read()
        .unwrap()
        .get(event.repository())
        .cloned()
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
        BorsRepositoryEvent::PullRequestEdited(payload) => {
            let span =
                tracing::info_span!("Pull request edited", repo = payload.repository.to_string());

            handle_pull_request_edited(repo, db, mergeable_queue_tx, payload)
                .instrument(span.clone())
                .await?;
        }
        BorsRepositoryEvent::PullRequestCommitPushed(payload) => {
            let span =
                tracing::info_span!("Pull request pushed", repo = payload.repository.to_string());

            handle_push_to_pull_request(repo, db, mergeable_queue_tx, payload)
                .instrument(span.clone())
                .await?;
        }
        BorsRepositoryEvent::PullRequestOpened(payload) => {
            let span =
                tracing::info_span!("Pull request opened", repo = payload.repository.to_string());

            handle_pull_request_opened(repo, db, mergeable_queue_tx, payload)
                .instrument(span.clone())
                .await?;
        }
        BorsRepositoryEvent::PullRequestClosed(payload) => {
            let span =
                tracing::info_span!("Pull request closed", repo = payload.repository.to_string());

            handle_pull_request_closed(repo, db, payload)
                .instrument(span.clone())
                .await?;
        }
        BorsRepositoryEvent::PullRequestMerged(payload) => {
            let span =
                tracing::info_span!("Pull request merged", repo = payload.repository.to_string());

            handle_pull_request_merged(repo, db, payload)
                .instrument(span.clone())
                .await?;
        }
        BorsRepositoryEvent::PullRequestReopened(payload) => {
            let span = tracing::info_span!(
                "Pull request reopened",
                repo = payload.repository.to_string()
            );

            handle_pull_request_reopened(repo, db, mergeable_queue_tx, payload)
                .instrument(span.clone())
                .await?;
        }
        BorsRepositoryEvent::PullRequestConvertedToDraft(payload) => {
            let span = tracing::info_span!(
                "Pull request converted to draft",
                repo = payload.repository.to_string()
            );

            handle_pull_request_converted_to_draft(repo, db, payload)
                .instrument(span.clone())
                .await?;
        }
        BorsRepositoryEvent::PullRequestReadyForReview(payload) => {
            let span = tracing::info_span!(
                "Pull request ready for review",
                repo = payload.repository.to_string()
            );

            handle_pull_request_ready_for_review(repo, db, payload)
                .instrument(span.clone())
                .await?;
        }
        BorsRepositoryEvent::PushToBranch(payload) => {
            let span =
                tracing::info_span!("Pushed to branch", repo = payload.repository.to_string());

            handle_push_to_branch(repo, db, mergeable_queue_tx, payload)
                .instrument(span.clone())
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub static WAIT_FOR_REPO_CONFIG_REFRESH: TestSyncMarker = TestSyncMarker::new();

/// This function executes a single BORS global event
pub async fn handle_bors_global_event(
    event: BorsGlobalEvent,
    ctx: Arc<BorsContext>,
    gh_client: &Octocrab,
    team_api_client: &TeamApiClient,
    mergeable_queue_tx: MergeableQueueSender,
) -> anyhow::Result<()> {
    let db = Arc::clone(&ctx.db);
    match event {
        BorsGlobalEvent::InstallationsChanged => {
            let span = tracing::info_span!("Installations changed");
            reload_repos(ctx, gh_client, team_api_client)
                .instrument(span)
                .await?;
        }
        BorsGlobalEvent::RefreshConfig => {
            let span = tracing::info_span!("Refresh configuration of repositories");
            let repos: Vec<Arc<RepositoryState>> =
                ctx.repositories.read().unwrap().values().cloned().collect();
            futures::future::join_all(repos.into_iter().map(|repo| {
                let span = tracing::info_span!(
                    "Refresh configuration",
                    repo = repo.repository().to_string()
                );
                reload_repository_config(repo).instrument(span)
            }))
            .instrument(span)
            .await;

            #[cfg(test)]
            WAIT_FOR_REPO_CONFIG_REFRESH.mark();
        }
        BorsGlobalEvent::RefreshPermissions => {}
        BorsGlobalEvent::CancelTimedOutBuilds => {}
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

    // Bail if no commands
    if commands.is_empty() {
        return Ok(());
    }

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
                    BorsCommand::Approve {
                        approver,
                        priority,
                        rollup,
                    } => {
                        let span = tracing::info_span!("Approve");
                        command_approve(
                            repo,
                            database,
                            &pull_request,
                            &comment.author,
                            &approver,
                            priority,
                            rollup,
                        )
                        .instrument(span)
                        .await
                    }
                    BorsCommand::OpenTree => {
                        let span = tracing::info_span!("TreeOpen");
                        command_open_tree(repo, database, &pull_request, &comment.author)
                            .instrument(span)
                            .await
                    }
                    BorsCommand::TreeClosed(priority) => {
                        let span = tracing::info_span!("TreeClosed");
                        command_close_tree(
                            repo,
                            database,
                            &pull_request,
                            &comment.author,
                            priority,
                            &comment.html_url,
                        )
                        .instrument(span)
                        .await
                    }
                    BorsCommand::Unapprove => {
                        let span = tracing::info_span!("Unapprove");
                        command_unapprove(repo, database, &pull_request, &comment.author)
                            .instrument(span)
                            .await
                    }
                    BorsCommand::SetPriority(priority) => {
                        let span = tracing::info_span!("Priority");
                        command_set_priority(
                            repo,
                            database,
                            &pull_request,
                            &comment.author,
                            priority,
                        )
                        .instrument(span)
                        .await
                    }
                    BorsCommand::SetDelegate(delegate_type) => {
                        let span = tracing::info_span!("Delegate");
                        command_delegate(
                            repo,
                            database,
                            &pull_request,
                            &comment.author,
                            delegate_type,
                        )
                        .instrument(span)
                        .await
                    }
                    BorsCommand::Undelegate => {
                        let span = tracing::info_span!("Undelegate");
                        command_undelegate(repo, database, &pull_request, &comment.author)
                            .instrument(span)
                            .await
                    }
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
                    BorsCommand::Info => {
                        let span = tracing::info_span!("Info");
                        command_info(repo, &pull_request, database)
                            .instrument(span)
                            .await
                    }
                    BorsCommand::SetRollupMode(rollup) => {
                        let span = tracing::info_span!("Rollup");
                        command_set_rollup(repo, database, &pull_request, &comment.author, rollup)
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
    gh_client: &Octocrab,
    team_api_client: &TeamApiClient,
) -> anyhow::Result<()> {
    let reloaded_repos = load_repositories(gh_client, team_api_client).await?;
    let mut repositories = ctx.repositories.write().unwrap();
    for repo in repositories.values() {
        if !reloaded_repos.contains_key(repo.repository()) {
            tracing::warn!("Repository {} was not reloaded", repo.repository());
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

/// Deny permission for a request.
async fn deny_request(
    repo: &RepositoryState,
    pr: &PullRequest,
    author: &GithubUser,
    permission_type: PermissionType,
) -> anyhow::Result<()> {
    tracing::warn!(
        "Permission denied for request command by {}",
        author.username
    );
    repo.client
        .post_comment(
            pr.number,
            Comment::new(format!(
                "@{}: :key: Insufficient privileges: not in {} users",
                author.username, permission_type
            )),
        )
        .await
}

/// Check if a user has specified permission or has been delegated.
async fn has_permission(
    repo_state: &RepositoryState,
    author: &GithubUser,
    pr: &PullRequest,
    db: &PgDbClient,
    permission: PermissionType,
) -> anyhow::Result<bool> {
    if repo_state
        .permissions
        .load()
        .has_permission(author.id, permission.clone())
    {
        return Ok(true);
    }

    let pr_model = db
        .upsert_pull_request(
            repo_state.repository(),
            pr.number,
            &pr.base.name,
            pr.mergeable_state.clone().into(),
            &pr.status,
        )
        .await?;

    if author.id != pr.author.id {
        return Ok(false);
    }

    let is_delegated = pr_model
        .delegated_permission
        .is_some_and(|perm| match permission {
            PermissionType::Review => matches!(perm, DelegatedPermission::Review),
            PermissionType::Try => {
                matches!(perm, DelegatedPermission::Try | DelegatedPermission::Review)
            }
        });

    Ok(is_delegated)
}

#[cfg(test)]
mod tests {
    use crate::tests::mocks::{Comment, User, run_test};

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

    #[sqlx::test]
    async fn do_not_load_pr_on_unrelated_comment(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.default_repo().lock().pull_request_error = true;
            tester.post_comment("no command").await?;
            Ok(tester)
        })
        .await;
    }
}
