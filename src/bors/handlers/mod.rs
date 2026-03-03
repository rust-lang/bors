use std::sync::Arc;

use crate::bors::command::{BorsCommand, CommandParseError};
use crate::bors::event::{BorsGlobalEvent, BorsRepositoryEvent, PullRequestComment};
use crate::bors::handlers::autobuild::{command_cancel, command_retry};
use crate::bors::handlers::help::command_help;
use crate::bors::handlers::info::command_info;
use crate::bors::handlers::ping::command_ping;
use crate::bors::handlers::pr_events::{
    handle_pull_request_assigned, handle_pull_request_unassigned,
};
use crate::bors::handlers::refresh::{
    reload_mergeability_status, reload_repository_config, reload_repository_permissions,
};
use crate::bors::handlers::review::{
    command_approve, command_close_tree, command_open_tree, command_unapprove,
};
use crate::bors::handlers::trybuild::{command_try_build, command_try_cancel};
use crate::bors::handlers::workflow::{
    AutoBuildCancelReason, handle_workflow_completed, handle_workflow_started,
    maybe_cancel_auto_build,
};
use crate::bors::labels::handle_label_trigger;
use crate::bors::mergeability_queue::set_pr_mergeability_based_on_user_action;
use crate::bors::process::QueueSenders;
use crate::bors::{
    AUTO_BRANCH_NAME, BorsContext, CommandPrefix, Comment, PullRequestStatus, RepositoryState,
    TRY_BRANCH_NAME,
};
use crate::database::{DelegatedPermission, PullRequestModel};
use crate::github::{GithubUser, LabelTrigger, PullRequest, PullRequestNumber};
use crate::permissions::PermissionType;
use crate::{PgDbClient, TeamApiClient, load_repositories};
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
use tracing::{Instrument, debug_span};

mod autobuild;
mod help;
mod info;
mod ping;
mod pr_events;
mod refresh;
mod review;
mod squash;
mod trybuild;
mod workflow;

/// This function executes a single bors repository event
pub async fn handle_bors_repository_event(
    event: BorsRepositoryEvent,
    ctx: Arc<BorsContext>,
    senders: QueueSenders,
) -> anyhow::Result<()> {
    let db = Arc::clone(&ctx.db);
    let Some(repo) = ctx.repositories.get(event.repository()) else {
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

            // Also ignore comments made by homu
            if comment.author.username == "bors" {
                tracing::trace!("Ignoring comment {comment:?} because it was authored by homu");
                return Ok(());
            }

            let span = tracing::info_span!(
                "Comment",
                pr = format!("{}#{}", comment.repository, comment.pr_number),
                author = comment.author.username
            );
            let pr_number = comment.pr_number;
            if let Err(error) =
                handle_comment(Arc::clone(&repo), db.clone(), ctx, comment, &senders)
                    .instrument(span.clone())
                    .await
            {
                repo.client
                    .post_comment(
                        pr_number,
                        Comment::new(
                            ":x: Encountered an error while executing command".to_string(),
                        ),
                        &db,
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
                let span = tracing::info_span!("Repo", "{}", repo.repository());
                reload_repository_config(repo).instrument(span)
            })
            .instrument(span)
            .await?;
        }
        BorsGlobalEvent::RefreshPermissions => {
            let span = tracing::info_span!("Refresh permissions");
            for_each_repo(&ctx, |repo| {
                let span = tracing::info_span!("Repo", "{}", repo.repository());
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
                let span = tracing::info_span!("Repo", "{}", repo.repository());
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
                let subspan = tracing::info_span!("Repo", "{}", repo.repository());
                sync_pull_requests_state(repo, Arc::clone(&db), senders.mergeability_queue())
                    .instrument(subspan)
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
    let repos: Vec<Arc<RepositoryState>> = ctx.repositories.repositories();
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
    senders: &QueueSenders,
) -> anyhow::Result<()> {
    use std::fmt::Write;

    let pr_number = comment.pr_number;
    let commands = ctx.parser.parse_commands(&comment.text);

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

    let mut pr_db = database
        .upsert_pull_request(repo.repository(), pr_github.clone().into())
        .await
        .with_context(|| format!("Cannot upsert PR {pr_number} into the database"))?;
    set_pr_mergeability_based_on_user_action(
        &database,
        &pr_github,
        &pr_db,
        senders.mergeability_queue(),
    )
    .await?;

    for (index, command) in commands.into_iter().enumerate() {
        match command {
            Ok(command) => {
                // Reload the PR state from DB, because a previous command might have changed it.
                if index > 0
                    && let Some(pr) = database
                        .get_pull_request(repo.repository(), pr_number)
                        .await?
                {
                    pr_db = pr;
                }

                let pr = PullRequestData {
                    github: &pr_github,
                    db: &pr_db,
                };

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
                            ctx.clone(),
                            repo,
                            database,
                            pr,
                            &comment.author,
                            &approver,
                            priority,
                            rollup,
                            senders.merge_queue(),
                        )
                        .instrument(span)
                        .await
                    }
                    BorsCommand::OpenTree => {
                        let span = tracing::info_span!("TreeOpen");
                        command_open_tree(
                            repo,
                            database,
                            pr,
                            &comment.author,
                            senders.merge_queue(),
                        )
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
                            senders.merge_queue(),
                        )
                        .instrument(span)
                        .await
                    }
                    BorsCommand::Unapprove => {
                        let span = tracing::info_span!("Unapprove");
                        command_unapprove(repo, database, pr, &comment.author, &comment.html_url)
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
                        command_help(repo, &ctx.db, pr.number())
                            .instrument(span)
                            .await
                    }
                    BorsCommand::Ping => {
                        let span = tracing::info_span!("Ping");
                        command_ping(repo, &ctx.db, pr.number())
                            .instrument(span)
                            .await
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
                    BorsCommand::Retry => {
                        let span = tracing::info_span!("Retry");
                        command_retry(
                            repo,
                            database,
                            pr,
                            &comment.author,
                            senders.merge_queue(),
                            ctx.parser.prefix(),
                        )
                        .instrument(span)
                        .await
                    }
                    BorsCommand::Cancel => {
                        let span = tracing::info_span!("Cancel");
                        command_cancel(
                            repo,
                            database,
                            pr,
                            &comment.author,
                            ctx.parser.prefix(),
                            senders.merge_queue(),
                        )
                        .instrument(span)
                        .await
                    }
                    BorsCommand::Squash { commit_message } => {
                        let span = tracing::info_span!("Squash");
                        if ctx.local_git_available() {
                            squash::command_squash(
                                repo,
                                database,
                                pr,
                                &comment.author,
                                commit_message,
                                ctx.parser.prefix(),
                                senders.gitops_queue(),
                            )
                            .instrument(span)
                            .await
                        } else {
                            repo.client
                                .post_comment(
                                    pr_number,
                                    Comment::new(
                                        "`@bors squash` is not enabled in this bors instance."
                                            .to_string(),
                                    ),
                                    &ctx.db,
                                )
                                .instrument(span)
                                .await?;
                            Ok(())
                        }
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
                    CommandParseError::UnknownArg { arg, did_you_mean } => {
                        format!(
                            r#"Unknown argument "{arg}". Did you mean to use `{} {did_you_mean}`?"#,
                            ctx.parser.prefix()
                        )
                    }
                    CommandParseError::DuplicateArg(arg) => {
                        format!(r#"Argument "{arg}" found multiple times."#)
                    }
                    CommandParseError::ValidationError(error) => {
                        format!("Invalid command: {error}.")
                    }
                    CommandParseError::UnclosedQuote => "Unclosed quote in argument.".to_string(),
                };
                writeln!(
                    message,
                    " Run `{} help` to see available commands.",
                    ctx.parser.prefix()
                )?;
                tracing::warn!("{}", message);
                repo.client
                    .post_comment(pr_github.number, Comment::new(message), &database)
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
    for repo in ctx.repositories.repositories() {
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

        if ctx.repositories.insert(repo) {
            tracing::info!("Repository {name} was added");
        } else {
            tracing::info!("Repository {name} was reloaded");
        }
    }
    Ok(())
}

/// Deny permission for a request.
async fn deny_request(
    repo: &RepositoryState,
    db: &PgDbClient,
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
            db,
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

pub struct InvalidationInfo {
    reason: InvalidationReason,
    /// URL to a comment that provides mores context about the invalidation.
    comment_url: Option<String>,
}

impl InvalidationInfo {
    pub fn new(reason: InvalidationReason) -> Self {
        Self {
            reason,
            comment_url: None,
        }
    }

    pub fn with_comment_url<C: Into<Option<String>>>(self, comment_url: C) -> Self {
        Self {
            comment_url: comment_url.into(),
            ..self
        }
    }
}

/// Why was a pull request invalidated?
#[derive(Debug, Clone)]
pub enum InvalidationReason {
    /// A new commit was pushed to the pull request.
    /// If it was approved, it will be unapproved.
    /// If it was contained in any rollups, they will be closed.
    CommitShaChanged,
    /// The pull request was closed.
    /// If it was approved, it will be unapproved.
    /// If it was contained in any rollups, they will be closed.
    Close,
    /// The pull request was unapproved, but its contents (HEAD SHA) should still be the same.
    Unapproval,
    /// A member of a rollup was invalidated.
    RollupMemberInvalidated {
        member: PullRequestNumber,
        reason: Box<InvalidationReason>,
    },
}

pub async fn unapprove_pr(
    repo_state: &RepositoryState,
    db: &PgDbClient,
    pr_db: &PullRequestModel,
    pr_gh: &PullRequest,
) -> anyhow::Result<()> {
    db.unapprove(pr_db).await?;
    handle_label_trigger(repo_state, pr_gh, LabelTrigger::Unapproved).await?;
    Ok(())
}

pub struct InvalidationComment {
    /// Start of the invalidation comment
    base_text: String,
    /// Should the comment be sent even if no unapproval happened and thus only `base_text`
    /// would be sent?
    post_always: bool,
}

impl InvalidationComment {
    pub fn new(base_text: String) -> Self {
        Self {
            base_text,
            post_always: false,
        }
    }

    pub fn post_always(self) -> Self {
        Self {
            post_always: true,
            ..self
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct InvalidationOutcome {
    unapproved: bool,
    closed: bool,
}

pub struct MaybeInvalidatedRollup {
    number: PullRequestNumber,
    outcome: InvalidationOutcome,
}

/// In several situations, we need to "invalidate" a pull request, which amounts to doing several
/// things:
///
/// 1. Unapprove the PR if it was approved, and apply the corresponding unapproval label trigger.
/// 2. Cancel a running auto build, if there was any.
/// 3. "Recursively" invalidate all rollups containing the given pull request.
///   - If a rollup is invalidated due to a member PR changing its commit SHA, it will be closed.
///
/// This function does all that.
///
/// `comment` may contain the start of a comment that will be posted on the PR if invalidation
/// actually happened.
/// If nothing else would be added to the `comment` text, no comment will be sent!
pub async fn invalidate_pr(
    repo_state: &RepositoryState,
    db: &PgDbClient,
    pr_db: &PullRequestModel,
    pr_gh: &PullRequest,
    info: InvalidationInfo,
    comment: Option<InvalidationComment>,
) -> anyhow::Result<InvalidationOutcome> {
    // Step 1: unapprove the pull request if it was approved
    // This happens everytime the PR is invalidated, if it was approved before
    let pr_unapproved = if pr_db.is_approved() {
        unapprove_pr(repo_state, db, pr_db, pr_gh).await?;
        true
    } else {
        false
    };

    fn get_cancel_reason(reason: &InvalidationReason) -> AutoBuildCancelReason {
        match reason {
            InvalidationReason::CommitShaChanged => AutoBuildCancelReason::PushToPR,
            InvalidationReason::Close => AutoBuildCancelReason::Close,
            InvalidationReason::Unapproval => AutoBuildCancelReason::Unapproval,
            InvalidationReason::RollupMemberInvalidated { reason, .. } => get_cancel_reason(reason),
        }
    }

    // Step 2: if there was a running auto build on the PR, cancel it
    let auto_build_cancel_message = maybe_cancel_auto_build(
        &repo_state.client,
        db,
        pr_db,
        get_cancel_reason(&info.reason),
    )
    .await?;

    // Step 3: close the rollup if its member was closed or received a new push
    // Note that we don't do this on `InvalidationReason::Close` itself, because that happens after
    // the PR has been closed already.
    let pr_closed = if let InvalidationReason::RollupMemberInvalidated { reason, .. } = &info.reason
        && let InvalidationReason::Close | InvalidationReason::CommitShaChanged = &**reason
        && matches!(
            pr_gh.status,
            PullRequestStatus::Open | PullRequestStatus::Draft
        ) {
        repo_state.client.close_pr(pr_gh.number).await?;
        true
    } else {
        matches!(info.reason, InvalidationReason::Close)
    };

    let comment_url = info.comment_url;

    // Step 4: recursively invalidate all open rollups containing this PR
    let invalidate_rollups = match info.reason {
        InvalidationReason::CommitShaChanged
        | InvalidationReason::Close
        | InvalidationReason::Unapproval => true,
        // We do not assume that rollups contain other rollups
        InvalidationReason::RollupMemberInvalidated { .. } => false,
    };
    let invalidated_rollups = if invalidate_rollups {
        let rollups = db
            .find_rollups_for_member_pr(pr_db)
            .await?
            .into_iter()
            // We do not deal with
            .filter(|rollup| match rollup.status {
                PullRequestStatus::Closed | PullRequestStatus::Merged => false,
                PullRequestStatus::Draft | PullRequestStatus::Open => true,
            })
            // Just to avoid weird edge cases, shouldn't happen ~ever
            .take(10)
            .collect::<Vec<_>>();

        let invalidate_rollup_futs = rollups.iter().map(|rollup_db| {
            let span =
                debug_span!("Invalidating rollup", rollup = rollup_db.number.0, reason = ?info.reason);
            let db = db.clone();
            let comment_url = comment_url.clone();
            let reason = info.reason.clone();
            async move {
                let rollup_pr = repo_state.client.get_pull_request(rollup_db.number).await?;
                let outcome = invalidate_pr(
                    repo_state,
                    &db,
                    rollup_db,
                    &rollup_pr,
                    InvalidationInfo::new(InvalidationReason::RollupMemberInvalidated {
                        member: pr_gh.number,
                        reason: Box::new(reason),
                    })
                        .with_comment_url(comment_url),
                    None,
                )
                    .await?;
                anyhow::Ok(MaybeInvalidatedRollup {
                    number: rollup_pr.number,
                    outcome
                })
            }
                .instrument(span)
        });

        let mut invalidated_rollups = vec![];
        for res in futures::future::join_all(invalidate_rollup_futs).await {
            match res {
                Ok(rollup) => {
                    invalidated_rollups.push(rollup);
                }
                Err(error) => {
                    tracing::error!("It was not possible to invalidate rollup: {error:?}");
                }
            }
        }
        invalidated_rollups
    } else {
        vec![]
    };

    let outcome = InvalidationOutcome {
        unapproved: pr_unapproved,
        closed: pr_closed,
    };
    if let Some(comment) = invalidation_comment(
        pr_db,
        auto_build_cancel_message,
        comment_url,
        comment,
        info.reason,
        invalidated_rollups,
        outcome,
    ) {
        repo_state
            .client
            .post_comment(pr_gh.number, comment, db)
            .await?;
    }

    Ok(outcome)
}

pub fn invalidation_comment(
    pr_db: &PullRequestModel,
    build_cancelled_msg: Option<String>,
    invalidation_comment_url: Option<String>,
    comment: Option<InvalidationComment>,
    reason: InvalidationReason,
    invalidated_rollups: Vec<MaybeInvalidatedRollup>,
    outcome: InvalidationOutcome,
) -> Option<Comment> {
    use itertools::Itertools;
    use std::fmt::Write;

    let mut invalidated_rollups: Vec<MaybeInvalidatedRollup> = invalidated_rollups
        .into_iter()
        .filter(|rollup| rollup.outcome.unapproved || rollup.outcome.closed)
        .collect();

    let mut msg = comment
        .as_ref()
        .map(|c| c.base_text.clone())
        .unwrap_or_default();

    let mut append = |fmt| {
        if !msg.is_empty() {
            msg.push_str("\n\n");
        }
        write!(msg, "{fmt}").unwrap();
    };

    // Rollup was invalidated
    let is_rollup = if let InvalidationReason::RollupMemberInvalidated { reason, member } = &reason
    {
        let wrap = |text: &str| {
            if let Some(comment_url) = invalidation_comment_url {
                format!("[{text}]({comment_url})")
            } else {
                text.to_string()
            }
        };

        let action = match &**reason {
            InvalidationReason::CommitShaChanged => format!("{} its commit SHA", wrap("changed")),
            InvalidationReason::Close => format!("was {}", wrap("closed")),
            InvalidationReason::Unapproval => format!("was {}", wrap("unapproved")),
            InvalidationReason::RollupMemberInvalidated { .. } => {
                format!("was {}", wrap("invalidated"))
            }
        };
        append(format!(
            "PR #{member}, which is a member of this rollup, {action}.",
        ));
        true
    } else {
        false
    };

    let pr_label = if is_rollup { "rollup" } else { "pull request" };
    // If we had an approved PR with a failed build, there's not much point in sending this warning
    let had_failed_build = pr_db
        .auto_build
        .as_ref()
        .map(|b| b.status.is_failure_or_cancel())
        .unwrap_or(false);

    // In this case we do not have to post a comment, to avoid needless spam
    // It would be nicer to solve this in some more robust way..
    let is_simple_unapproval = matches!(reason, InvalidationReason::Unapproval)
        && !outcome.closed
        && build_cancelled_msg.is_none()
        && invalidated_rollups.is_empty();

    if outcome.unapproved && !is_simple_unapproval && (!had_failed_build || outcome.closed) {
        append(format!(
            "This {pr_label} was{} unapproved{}.",
            if is_rollup { " thus" } else { "" },
            if outcome.closed {
                " due to being closed"
            } else {
                ""
            }
        ));
    }

    // Rollup member was invalidated
    if !invalidated_rollups.is_empty() {
        let action = |outcome: InvalidationOutcome| {
            if outcome.closed {
                "closed"
            } else if outcome.unapproved {
                "unapproved"
            } else {
                "invalidated"
            }
        };

        if let [rollup] = invalidated_rollups.as_slice() {
            append(format!(
                "This PR was contained in a rollup (#{}), which was {}.",
                rollup.number,
                action(rollup.outcome)
            ));
        } else {
            invalidated_rollups.sort_by_key(|pr| pr.number);
            append(format!(
                "This PR was contained in the following rollups:\n{}",
                invalidated_rollups
                    .into_iter()
                    .map(|rollup| format!("- #{} was {}", rollup.number, action(rollup.outcome)))
                    .join("\n")
            ));
        }
    }

    // Auto build was cancelled
    if let Some(cancel_msg) = build_cancelled_msg {
        append(cancel_msg.to_string());
    }

    let post_always = comment.as_ref().map(|c| c.post_always).unwrap_or(false);

    // If we have nothing to post or the comment equals the base comment and post_always is false,
    // do not send any comment.
    if msg.is_empty() || (msg == comment.map(|c| c.base_text).unwrap_or_default() && !post_always) {
        None
    } else {
        Some(Comment::new(msg))
    }
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
