use crate::bors::event::BorsEvent;
use crate::bors::merge_queue::{MergeQueueSender, start_merge_queue};
use crate::bors::mergeability_queue::{
    MergeabilityQueueReceiver, MergeabilityQueueSender, check_mergeability,
    create_mergeability_queue,
};
use crate::bors::{
    BorsContext, CommandPrefix, RepositoryState, RollupMode, handle_bors_global_event,
    handle_bors_repository_event,
};
use crate::database::QueueStatus;
use crate::github::webhook::GitHubWebhook;
use crate::github::webhook::WebhookSecret;
use crate::templates::{
    HelpTemplate, HtmlTemplate, NotFoundTemplate, PullRequestStats, QueueTemplate, RepositoryView,
};
use crate::{BorsGlobalEvent, BorsRepositoryEvent, PgDbClient, TeamApiClient};

use super::AppError;
use super::GithubRepoName;
use super::rollup;
use crate::utils::sort_queue::sort_queue_prs;
use anyhow::Error;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use octocrab::Octocrab;
use secrecy::{ExposeSecret, SecretString};
use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::catch_panic::CatchPanicLayer;
use tracing::{Instrument, Span};

#[derive(Clone)]
pub struct OAuthConfig {
    client_id: String,
    client_secret: SecretString,
}

impl OAuthConfig {
    pub fn new(client_id: String, client_secret: String) -> Self {
        Self {
            client_id,
            client_secret: client_secret.into(),
        }
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn client_secret(&self) -> &str {
        self.client_secret.expose_secret()
    }
}

/// Shared server state for all axum handlers.
pub struct ServerState {
    repository_event_queue: mpsc::Sender<BorsRepositoryEvent>,
    global_event_queue: mpsc::Sender<BorsGlobalEvent>,
    webhook_secret: WebhookSecret,
    pub(super) oauth: Option<OAuthConfig>,
    repositories: HashMap<GithubRepoName, Arc<RepositoryState>>,
    pub(super) db: Arc<PgDbClient>,
    cmd_prefix: CommandPrefix,
}

impl ServerState {
    pub fn new(
        repository_event_queue: mpsc::Sender<BorsRepositoryEvent>,
        global_event_queue: mpsc::Sender<BorsGlobalEvent>,
        webhook_secret: WebhookSecret,
        oauth: Option<OAuthConfig>,
        repositories: HashMap<GithubRepoName, Arc<RepositoryState>>,
        db: Arc<PgDbClient>,
        cmd_prefix: CommandPrefix,
    ) -> Self {
        Self {
            repository_event_queue,
            global_event_queue,
            webhook_secret,
            oauth,
            repositories,
            db,
            cmd_prefix,
        }
    }

    pub fn get_webhook_secret(&self) -> &WebhookSecret {
        &self.webhook_secret
    }

    pub fn get_cmd_prefix(&self) -> &CommandPrefix {
        &self.cmd_prefix
    }
}

pub type ServerStateRef = Arc<ServerState>;

pub fn create_app(state: ServerState) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/help", get(help_handler))
        .route("/queue/{repo_name}", get(queue_handler))
        .route("/github", post(github_webhook_handler))
        .route("/health", get(health_handler))
        .route("/oauth/callback", get(rollup::oauth_callback_handler))
        .layer(ConcurrencyLimitLayer::new(100))
        .layer(CatchPanicLayer::custom(handle_panic))
        .with_state(Arc::new(state))
        .fallback(not_found_handler)
}

fn handle_panic(err: Box<dyn Any + Send + 'static>) -> Response {
    tracing::error!("Router panicked: {err:?}");
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
}

async fn not_found_handler() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, HtmlTemplate(NotFoundTemplate {}))
}

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, "")
}

async fn index_handler(State(state): State<ServerStateRef>) -> impl IntoResponse {
    // If we manage exactly one repo, redirect to its queue page directly
    if let Some(repo_name) = state.repositories.keys().next()
        && state.repositories.len() == 1
    {
        return Redirect::temporary(&format!("/queue/{}", repo_name.name)).into_response();
    }
    help_handler(State(state)).await.into_response()
}

async fn help_handler(State(state): State<ServerStateRef>) -> impl IntoResponse {
    let mut repos = Vec::with_capacity(state.repositories.len());
    for repo in state.repositories.keys() {
        let treeclosed = state
            .db
            .repo_db(repo)
            .await
            .ok()
            .flatten()
            .is_some_and(|repo| repo.tree_state.is_closed());
        repos.push(RepositoryView {
            name: repo.name().to_string(),
            treeclosed,
        });
    }

    HtmlTemplate(HelpTemplate {
        repos,
        cmd_prefix: state.get_cmd_prefix().as_ref().to_string(),
    })
}

pub async fn queue_handler(
    Path(repo_name): Path<String>,
    State(state): State<ServerStateRef>,
) -> Result<impl IntoResponse, AppError> {
    let repo = match state.db.repo_by_name(&repo_name).await? {
        Some(repo) => repo,
        None => {
            return Ok((
                StatusCode::NOT_FOUND,
                format!("Repository {repo_name} not found"),
            )
                .into_response());
        }
    };

    let prs = state.db.get_nonclosed_pull_requests(&repo.name).await?;
    let prs = sort_queue_prs(prs);

    let (in_queue_count, failed_count, rolled_up_count) =
        prs.iter()
            .fold((0, 0, 0), |(in_queue, failed, rolled_up), pr| {
                let (in_queue_inc, failed_inc) = match pr.queue_status() {
                    QueueStatus::Approved(..) => (1, 0),
                    QueueStatus::ReadyForMerge(..) => (1, 0),
                    QueueStatus::Pending(..) => (1, 0),
                    QueueStatus::Stalled(..) => (0, 1),
                    QueueStatus::NotApproved => (0, 0),
                };

                (
                    in_queue + in_queue_inc,
                    failed + failed_inc,
                    rolled_up + usize::from(matches!(pr.rollup, Some(RollupMode::Always))),
                )
            });

    Ok(HtmlTemplate(QueueTemplate {
        oauth_client_id: state
            .oauth
            .as_ref()
            .map(|config| config.client_id().to_string()),
        repo_name: repo.name.name().to_string(),
        repo_owner: repo.name.owner().to_string(),
        repo_url: format!("https://github.com/{}", repo.name),
        tree_state: repo.tree_state,
        stats: PullRequestStats {
            total_count: prs.len(),
            in_queue_count,
            failed_count,
            rolled_up_count,
        },
        prs,
    })
    .into_response())
}

/// Axum handler that receives a webhook and sends it to a webhook channel.
pub async fn github_webhook_handler(
    State(state): State<ServerStateRef>,
    GitHubWebhook(event): GitHubWebhook,
) -> impl IntoResponse {
    match event {
        BorsEvent::Global(e) => match state.global_event_queue.send(e).await {
            Ok(_) => (StatusCode::OK, ""),
            Err(err) => {
                tracing::error!("Could not send webhook global event: {err:?}");
                (StatusCode::INTERNAL_SERVER_ERROR, "")
            }
        },
        BorsEvent::Repository(e) => match state.repository_event_queue.send(e).await {
            Ok(_) => (StatusCode::OK, ""),
            Err(err) => {
                tracing::error!("Could not send webhook repository event: {err:?}");
                (StatusCode::INTERNAL_SERVER_ERROR, "")
            }
        },
    }
}

pub struct BorsProcess {
    pub repository_tx: mpsc::Sender<BorsRepositoryEvent>,
    pub global_tx: mpsc::Sender<BorsGlobalEvent>,
    pub merge_queue_tx: MergeQueueSender,
    pub mergeability_queue_tx: MergeabilityQueueSender,
    pub bors_process: Pin<Box<dyn Future<Output = ()> + Send>>,
}

/// Creates a future with a Bors process that continuously receives webhook events and reacts to
/// them.
pub fn create_bors_process(
    ctx: BorsContext,
    gh_client: Octocrab,
    team_api: TeamApiClient,
    merge_queue_max_interval: chrono::Duration,
) -> BorsProcess {
    let (repository_tx, repository_rx) = mpsc::channel::<BorsRepositoryEvent>(1024);
    let (global_tx, global_rx) = mpsc::channel::<BorsGlobalEvent>(1024);
    let (mergeability_queue_tx, mergeability_queue_rx) = create_mergeability_queue();
    let mergeability_queue_tx2 = mergeability_queue_tx.clone();

    let ctx = Arc::new(ctx);

    let (merge_queue_tx, merge_queue_fut) =
        start_merge_queue(ctx.clone(), merge_queue_max_interval);
    let merge_queue_tx2 = merge_queue_tx.clone();

    let service = async move {
        // In tests, we shutdown these futures by dropping the channel sender,
        // In that case, we need to wait until both of these futures resolve,
        // to make sure that they are able to handle all the events in the queue
        // before finishing.
        #[cfg(test)]
        {
            let _ = tokio::join!(
                consume_repository_events(
                    ctx.clone(),
                    repository_rx,
                    mergeability_queue_tx2.clone(),
                    merge_queue_tx2.clone()
                ),
                consume_global_events(
                    ctx.clone(),
                    global_rx,
                    mergeability_queue_tx2,
                    merge_queue_tx2,
                    gh_client,
                    team_api
                ),
                consume_mergeability_queue(ctx.clone(), mergeability_queue_rx),
                merge_queue_fut
            );
        }
        // In real execution, the bot runs forever. If there is something that finishes
        // the futures early, it's essentially a bug.
        // FIXME: maybe we could just use join for both versions of the code.
        #[cfg(not(test))]
        {
            tokio::select! {
                _ = consume_repository_events(ctx.clone(), repository_rx, mergeability_queue_tx2.clone(), merge_queue_tx2.clone()) => {
                    tracing::error!("Repository event handling process has ended");
                }
                _ = consume_global_events(ctx.clone(), global_rx, mergeability_queue_tx2, merge_queue_tx2, gh_client, team_api) => {
                    tracing::error!("Global event handling process has ended");
                }
                _ = consume_mergeability_queue(ctx.clone(), mergeability_queue_rx) => {
                    tracing::error!("Mergeability queue handling process has ended")
                }
                _ = merge_queue_fut => {
                    tracing::error!("Merge queue handling process has ended");
                }
            }
        }
    };

    BorsProcess {
        repository_tx,
        global_tx,
        mergeability_queue_tx,
        merge_queue_tx,
        bors_process: Box::pin(service),
    }
}

async fn consume_repository_events(
    ctx: Arc<BorsContext>,
    mut repository_rx: mpsc::Receiver<BorsRepositoryEvent>,
    mergeability_queue_tx: MergeabilityQueueSender,
    merge_queue_tx: MergeQueueSender,
) {
    while let Some(event) = repository_rx.recv().await {
        let ctx = ctx.clone();
        let mergeability_queue_tx = mergeability_queue_tx.clone();

        let span = tracing::info_span!("RepositoryEvent");
        tracing::debug!("Received repository event: {event:?}");
        if let Err(error) =
            handle_bors_repository_event(event, ctx, mergeability_queue_tx, merge_queue_tx.clone())
                .instrument(span.clone())
                .await
        {
            handle_root_error(span, error);
        }
    }
}

async fn consume_global_events(
    ctx: Arc<BorsContext>,
    mut global_rx: mpsc::Receiver<BorsGlobalEvent>,
    mergeability_queue_tx: MergeabilityQueueSender,
    merge_queue_tx: MergeQueueSender,
    gh_client: Octocrab,
    team_api: TeamApiClient,
) {
    while let Some(event) = global_rx.recv().await {
        let ctx = ctx.clone();
        let mergeability_queue_tx = mergeability_queue_tx.clone();
        let merge_queue_tx = merge_queue_tx.clone();

        let span = tracing::info_span!("GlobalEvent");
        tracing::trace!("Received global event: {event:?}");
        if let Err(error) = handle_bors_global_event(
            event,
            ctx,
            &gh_client,
            &team_api,
            mergeability_queue_tx,
            merge_queue_tx,
        )
        .instrument(span.clone())
        .await
        {
            handle_root_error(span, error);
        }
    }
}

async fn consume_mergeability_queue(
    ctx: Arc<BorsContext>,
    mergeability_queue_rx: MergeabilityQueueReceiver,
) {
    while let Some((mq_item, mq_tx)) = mergeability_queue_rx.dequeue().await {
        let ctx = ctx.clone();

        let span = tracing::debug_span!(
            "Mergeability check",
            repo = mq_item.pull_request.repo.to_string(),
            pr = mq_item.pull_request.pr_number.0,
            attempt = mq_item.attempt
        );
        if let Err(error) = check_mergeability(ctx, mq_tx, mq_item)
            .instrument(span.clone())
            .await
        {
            handle_root_error(span, error);
        }
    }
}

#[allow(unused_variables)]
fn handle_root_error(span: Span, error: Error) {
    // In tests, we want to panic on all errors.
    #[cfg(test)]
    {
        panic!("Handler failed: {error:?}");
    }
    #[cfg(not(test))]
    {
        use crate::utils::logging::LogError;
        span.log_error(error);
    }
}
