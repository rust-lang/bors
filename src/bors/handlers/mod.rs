use std::sync::Arc;

use crate::bors::command::{BorsCommand, CommandParseError};
use crate::bors::event::{BorsGlobalEvent, BorsRepositoryEvent, PullRequestComment};
use crate::bors::handlers::help::command_help;
use crate::bors::handlers::info::command_info;
use crate::bors::handlers::ping::command_ping;
use crate::bors::handlers::pr_events::{
    handle_pull_request_assigned, handle_pull_request_unassigned,
};
use crate::bors::handlers::refresh::{
    reload_mergeability_status, reload_repository_config, reload_repository_permissions,
};
use crate::bors::handlers::retry::command_retry;
use crate::bors::handlers::review::{
    command_approve, command_close_tree, command_open_tree, command_unapprove,
};
use crate::bors::handlers::trybuild::{command_try_build, command_try_cancel};
use crate::bors::handlers::workflow::{handle_workflow_completed, handle_workflow_started};
use crate::bors::labels::handle_label_trigger;
use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::process::QueueSenders;
use crate::bors::{
    AUTO_BRANCH_NAME, BorsContext, CommandPrefix, Comment, RepositoryState, TRY_BRANCH_NAME,
};
use crate::database::{DelegatedPermission, PullRequestModel};
use crate::github::{GithubUser, LabelTrigger, PullRequest, PullRequestNumber};
use crate::permissions::PermissionType;
use crate::{CommandParser, PgDbClient, TeamApiClient, load_repositories};
use anyhow::Context;
use futures::TryFutureExt;
use octocrab::Octocrab;
use pr_events::{
    handle_pull_request_closed, handle_pull_request_converted_to_draft, handle_pull_request_edited,
    handle_pull_request_merged, handle_pull_request_opened, handle_pull_request_ready_for_review,
    handle_pull_request_reopened, handle_push_to_branch, handle_push_to_pull_request,
};
use refresh::sync_pull_requests_state;
use review::{command_delegate, command_set_priority, command_set_rollup, command_undelegate};
use tracing::Instrument;

mod help;
mod info;
mod ping;
mod pr_events;
mod refresh;
mod retry;
mod review;
mod trybuild;
mod workflow;

/// This function executes a single bors repository event
pub async fn handle_bors_repository_event(
    event: BorsRepositoryEvent,
    ctx: Arc<BorsContext>,
    senders: QueueSenders,
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
            if let Err(error) =
                handle_comment(Arc::clone(&repo), db, ctx, comment, senders.merge_queue())
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
            handle_workflow_started(repo, db, payload)
                .instrument(span.clone())
                .await?;

            #[cfg(test)]
            super::WAIT_FOR_WORKFLOW_STARTED.mark();
        }
        BorsRepositoryEvent::WorkflowCompleted(payload) => {
            let span = tracing::info_span!(
                "Workflow completed",
                repo = payload.repository.to_string(),
                id = payload.run_id.into_inner()
            );
            handle_workflow_completed(repo, db, payload, senders.build_queue())
                .instrument(span.clone())
                .await?;

            #[cfg(test)]
            super::WAIT_FOR_WORKFLOW_COMPLETED.mark();
        }
        BorsRepositoryEvent::PullRequestEdited(payload) => {
            let span =
                tracing::info_span!("Pull request edited", repo = payload.repository.to_string());

            handle_pull_request_edited(repo, db, senders.mergeability_queue(), payload)
                .instrument(span.clone())
                .await?;
        }
        BorsRepositoryEvent::PullRequestCommitPushed(payload) => {
            let span =
                tracing::info_span!("Pull request pushed", repo = payload.repository.to_string());

            handle_push_to_pull_request(repo, db, senders.mergeability_queue(), payload)
                .instrument(span.clone())
                .await?;
        }
        BorsRepositoryEvent::PullRequestOpened(payload) => {
            let span =
                tracing::info_span!("Pull request opened", repo = payload.repository.to_string());

            handle_pull_request_opened(repo, db, ctx, &senders, payload)
                .instrument(span.clone())
                .await?;

            #[cfg(test)]
            super::WAIT_FOR_PR_OPEN.mark();
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

            handle_pull_request_reopened(repo, db, senders.mergeability_queue(), payload)
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
        BorsRepositoryEvent::PullRequestAssigned(payload) => {
            let span = tracing::info_span!(
                "Pull request assigned",
                repo = payload.repository.to_string()
            );

            handle_pull_request_assigned(repo, db, payload)
                .instrument(span.clone())
                .await?;
        }
        BorsRepositoryEvent::PullRequestUnassigned(payload) => {
            let span = tracing::info_span!(
                "Pull request unassigned",
                repo = payload.repository.to_string()
            );

            handle_pull_request_unassigned(repo, db, payload)
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

            handle_push_to_branch(repo, db, senders.mergeability_queue(), payload)
                .instrument(span.clone())
                .await?;
        }
    }
    Ok(())
}

/// This function executes a single BORS global event
pub async fn handle_bors_global_event(
    event: BorsGlobalEvent,
    ctx: Arc<BorsContext>,
    gh_client: &Octocrab,
    team_api_client: &TeamApiClient,
    senders: QueueSenders,
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
            let span = tracing::info_span!("Refresh config");
            for_each_repo(&ctx, |repo| {
                let span = tracing::info_span!("Repo", repo = repo.repository().to_string());
                reload_repository_config(repo).instrument(span)
            })
            .instrument(span)
            .await?;
        }
        BorsGlobalEvent::RefreshPermissions => {
            let span = tracing::info_span!("Refresh permissions");
            for_each_repo(&ctx, |repo| {
                let span = tracing::info_span!("Repo", repo = repo.repository().to_string());
                reload_repository_permissions(repo, team_api_client).instrument(span)
            })
            .instrument(span)
            .await?;
        }
        BorsGlobalEvent::RefreshPendingBuilds => {
            let span = tracing::info_span!("Refresh pending builds");
            for_each_repo(&ctx, |repo| {
                senders
                    .build_queue()
                    .refresh_pending_builds(repo.repository().clone())
                    .instrument(span.clone())
                    .map_err(|e| e.into())
            })
            .instrument(span.clone())
            .await?;
        }
        BorsGlobalEvent::RefreshPullRequestMergeability => {
            let span = tracing::info_span!("Refresh PR mergeability status");
            for_each_repo(&ctx, |repo| {
                let span = tracing::info_span!("Repo", repo = repo.repository().to_string());
                reload_mergeability_status(repo, &db, senders.mergeability_queue().clone())
                    .instrument(span)
            })
            .instrument(span)
            .await?;

            #[cfg(test)]
            crate::bors::WAIT_FOR_MERGEABILITY_STATUS_REFRESH.mark();
        }
        BorsGlobalEvent::RefreshPullRequestState => {
            let span = tracing::info_span!("Refresh PR status");
            for_each_repo(&ctx, |repo| {
                let subspan = tracing::info_span!("Repo", repo = repo.repository().to_string());
                sync_pull_requests_state(repo, Arc::clone(&db)).instrument(subspan)
            })
            .instrument(span)
            .await?;

            #[cfg(test)]
            crate::bors::WAIT_FOR_PR_STATUS_REFRESH.mark();
        }
        BorsGlobalEvent::ProcessMergeQueue => {
            senders.merge_queue().maybe_perform_tick().await?;
        }
    }
    Ok(())
}

/// Perform an asynchronous operation created by `make_fut` for each repository in parallel.
async fn for_each_repo<MakeFut, Fut>(ctx: &BorsContext, make_fut: MakeFut) -> anyhow::Result<()>
where
    MakeFut: Fn(Arc<RepositoryState>) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let repos: Vec<Arc<RepositoryState>> =
        ctx.repositories.read().unwrap().values().cloned().collect();
    futures::future::join_all(repos.into_iter().map(make_fut))
        .await
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(())
}

#[derive(Copy, Clone)]
pub struct PullRequestData<'a> {
    pub github: &'a PullRequest,
    pub db: &'a PullRequestModel,
}

impl PullRequestData<'_> {
    pub fn number(&self) -> PullRequestNumber {
        self.db.number
    }
}

async fn handle_comment(
    repo: Arc<RepositoryState>,
    database: Arc<PgDbClient>,
    ctx: Arc<BorsContext>,
    comment: PullRequestComment,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<()> {
    use std::fmt::Write;

    let pr_number = comment.pr_number;
    let mut commands = ctx.parser.parse_commands(&comment.text);

    // Temporary special case for migration from homu on rust-lang/rust.
    // Try to parse `@bors try` commands with a hardcoded prefix normally assigned to homu.
    if ctx.parser.prefix().as_ref() != "@bors" {
        let homu_commands = CommandParser::new_try_only(CommandPrefix::from("@bors".to_string()))
            .parse_commands(&comment.text)
            .into_iter()
            .filter(|res| match res {
                // Let homu handle unknown and missing commands
                Err(CommandParseError::UnknownCommand(_) | CommandParseError::MissingCommand) => {
                    false
                }
                _ => true,
            });
        commands.extend(homu_commands);
    }

    // Bail if no commands
    if commands.is_empty() {
        return Ok(());
    }

    tracing::debug!("Commands: {commands:?}");
    tracing::trace!("Text: {}", comment.text);

    let pr_github = repo
        .client
        .get_pull_request(pr_number)
        .await
        .with_context(|| format!("Cannot get information about PR {pr_number}"))?;

    for command in commands {
        match command {
            Ok(command) => {
                // Reload the PR state from DB, because a previous command might have changed it.
                let pr_db = database
                    .upsert_pull_request(repo.repository(), pr_github.clone().into())
                    .await
                    .with_context(|| format!("Cannot upsert PR {pr_number} into the database"))?;

                let pr = PullRequestData {
                    github: &pr_github,
                    db: &pr_db,
                };

                let repo = Arc::clone(&repo);
                let database = Arc::clone(&database);
                let result = match command {
                    BorsCommand::Retry => {
                        let span = tracing::info_span!("Retry");
                        command_retry(repo, database, pr, &comment.author, merge_queue_tx)
                            .instrument(span)
                            .await
                    }
                    BorsCommand::Approve {
                        approver,
                        priority,
                        rollup,
                    } => {
                        let span = tracing::info_span!("Approve");
                        command_approve(
                            ctx.clone(),
                            repo,
                            database,
                            pr,
                            &comment.author,
                            &approver,
                            priority,
                            rollup,
                            merge_queue_tx,
                        )
                        .instrument(span)
                        .await
                    }
                    BorsCommand::OpenTree => {
                        let span = tracing::info_span!("TreeOpen");
                        command_open_tree(repo, database, pr, &comment.author, merge_queue_tx)
                            .instrument(span)
                            .await
                    }
                    BorsCommand::TreeClosed(priority) => {
                        let span = tracing::info_span!("TreeClosed");
                        command_close_tree(
                            repo,
                            database,
                            pr,
                            &comment.author,
                            priority,
                            &comment.html_url,
                            merge_queue_tx,
                        )
                        .instrument(span)
                        .await
                    }
                    BorsCommand::Unapprove => {
                        let span = tracing::info_span!("Unapprove");
                        command_unapprove(repo, database, pr, &comment.author)
                            .instrument(span)
                            .await
                    }
                    BorsCommand::SetPriority(priority) => {
                        let span = tracing::info_span!("Priority");
                        command_set_priority(repo, database, pr, &comment.author, priority)
                            .instrument(span)
                            .await
                    }
                    BorsCommand::SetDelegate(delegate_type) => {
                        let span = tracing::info_span!("Delegate");
                        command_delegate(
                            repo,
                            database,
                            pr,
                            &comment.author,
                            delegate_type,
                            ctx.parser.prefix(),
                        )
                        .instrument(span)
                        .await
                    }
                    BorsCommand::Undelegate => {
                        let span = tracing::info_span!("Undelegate");
                        command_undelegate(repo, database, pr, &comment.author)
                            .instrument(span)
                            .await
                    }
                    BorsCommand::Help => {
                        let span = tracing::info_span!("Help");
                        command_help(repo, pr.number()).instrument(span).await
                    }
                    BorsCommand::Ping => {
                        let span = tracing::info_span!("Ping");
                        command_ping(repo, pr.number()).instrument(span).await
                    }
                    BorsCommand::Try { parent, jobs } => {
                        let span = tracing::info_span!("Try");
                        // we hard code the command prefix instead of using `ctx.parser.prefix()`
                        // because we are using the new bors for try builds, so we don't want to
                        // suggest using the `@bors2` prefix.
                        let command_prefix: CommandPrefix = "@bors".to_string().into();
                        command_try_build(
                            repo,
                            database,
                            pr,
                            &comment.author,
                            parent,
                            jobs,
                            &command_prefix,
                        )
                        .instrument(span)
                        .await
                    }
                    BorsCommand::TryCancel => {
                        let span = tracing::info_span!("Cancel try");
                        command_try_cancel(repo, database, pr, &comment.author)
                            .instrument(span)
                            .await
                    }
                    BorsCommand::Info => {
                        let span = tracing::info_span!("Info");
                        command_info(repo, pr, database).instrument(span).await
                    }
                    BorsCommand::SetRollupMode(rollup) => {
                        let span = tracing::info_span!("Rollup");
                        command_set_rollup(repo, database, pr, &comment.author, rollup)
                            .instrument(span)
                            .await
                    }
                };
                if result.is_err() {
                    return result.context("Cannot execute Bors command");
                }
            }
            Err(error) => {
                let mut message = match error {
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
                        format!("Invalid command: {error}.")
                    }
                };
                writeln!(
                    message,
                    " Run `{} help` to see available commands.",
                    ctx.parser.prefix()
                )?;
                tracing::warn!("{}", message);
                repo.client
                    .post_comment(pr_github.number, Comment::new(message))
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

/// Deny permission for a request.
async fn deny_request(
    repo: &RepositoryState,
    pr_number: PullRequestNumber,
    author: &GithubUser,
    permission_type: PermissionType,
) -> anyhow::Result<()> {
    tracing::warn!(
        "Permission denied for request command by {}",
        author.username
    );
    repo.client
        .post_comment(
            pr_number,
            Comment::new(format!(
                "@{}: :key: Insufficient privileges: not in {} users",
                author.username, permission_type
            )),
        )
        .await?;
    Ok(())
}

/// Check if a user has specified permission or has been delegated.
async fn has_permission(
    repo_state: &RepositoryState,
    user: &GithubUser,
    pr: PullRequestData<'_>,
    permission: PermissionType,
) -> anyhow::Result<bool> {
    if repo_state
        .permissions
        .load()
        .has_permission(user.id, permission.clone())
    {
        return Ok(true);
    }

    if user.id != pr.github.author.id {
        return Ok(false);
    }

    let is_delegated = pr
        .db
        .delegated_permission
        .is_some_and(|perm| match permission {
            PermissionType::Review => matches!(perm, DelegatedPermission::Review),
            PermissionType::Try => {
                matches!(perm, DelegatedPermission::Try | DelegatedPermission::Review)
            }
        });

    Ok(is_delegated)
}

/// Unapprove a PR in the DB and apply the corresponding label trigger.
pub async fn unapprove_pr(
    repo_state: &RepositoryState,
    db: &PgDbClient,
    pr: &PullRequestModel,
) -> anyhow::Result<()> {
    db.unapprove(pr).await?;
    handle_label_trigger(repo_state, pr.number, LabelTrigger::Unapproved).await
}

/// Is this branch interesting for the bot?
fn is_bors_observed_branch(branch: &str) -> bool {
    branch == TRY_BRANCH_NAME || branch == AUTO_BRANCH_NAME
}

#[cfg(test)]
mod tests {
    use crate::tests::{BorsTester, Comment, User, run_test};

    #[sqlx::test]
    async fn ignore_bot_comment(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.post_comment(Comment::from("@bors ping").with_author(User::bors_bot()))
                .await?;
            // Returning here will make sure that no comments were received
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn do_not_load_pr_on_unrelated_comment(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.modify_repo((), |repo| repo.pull_request_error = true);
            ctx.post_comment("no command").await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unknown_command(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.post_comment(Comment::from("@bors foo")).await?;
            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @r#"Unknown command "foo". Run `@bors help` to see available commands."#);
            Ok(())
        })
        .await;
    }
}
