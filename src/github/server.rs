use crate::bors::event::BorsEvent;
use crate::bors::merge_queue::{MergeQueueEvent, handle_merge_queue};
use crate::bors::mergeable_queue::{
    MergeableQueueReceiver, MergeableQueueSender, create_mergeable_queue,
    handle_mergeable_queue_item,
};
use crate::bors::{
    BorsContext, RepositoryState, RollupMode, handle_bors_global_event,
    handle_bors_repository_event,
};
use crate::github::webhook::GitHubWebhook;
use crate::github::webhook::WebhookSecret;
use crate::templates::{
    HelpTemplate, HtmlTemplate, NotFoundTemplate, PullRequestStats, QueueTemplate, RepositoryView,
};
use crate::{BorsGlobalEvent, BorsRepositoryEvent, PgDbClient, TeamApiClient};

use super::AppError;
use anyhow::Error;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use octocrab::Octocrab;
use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::catch_panic::CatchPanicLayer;
use tracing::{Instrument, Span};

use super::GithubRepoName;

/// Shared server state for all axum handlers.
pub struct ServerState {
    repository_event_queue: mpsc::Sender<BorsRepositoryEvent>,
    global_event_queue: mpsc::Sender<BorsGlobalEvent>,
    webhook_secret: WebhookSecret,
    repositories: HashMap<GithubRepoName, Arc<RepositoryState>>,
    db: Arc<PgDbClient>,
}

impl ServerState {
    pub fn new(
        repository_event_queue: mpsc::Sender<BorsRepositoryEvent>,
        global_event_queue: mpsc::Sender<BorsGlobalEvent>,
        webhook_secret: WebhookSecret,
        repositories: HashMap<GithubRepoName, Arc<RepositoryState>>,
        db: Arc<PgDbClient>,
    ) -> Self {
        Self {
            repository_event_queue,
            global_event_queue,
            webhook_secret,
            repositories,
            db,
        }
    }

    pub fn get_webhook_secret(&self) -> &WebhookSecret {
        &self.webhook_secret
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

async fn index_handler() -> impl IntoResponse {
    Redirect::permanent("/queue/rust")
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

    HtmlTemplate(HelpTemplate { repos })
}

async fn queue_handler(
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

    // TODO: add failed count
    let (approved_count, rolled_up_count) = prs.iter().fold((0, 0), |(approved, rolled_up), pr| {
        let is_approved = if pr.is_approved() { 1 } else { 0 };
        let is_rolled_up = if matches!(pr.rollup, Some(RollupMode::Always)) {
            1
        } else {
            0
        };
        (approved + is_approved, rolled_up + is_rolled_up)
    });

    Ok(HtmlTemplate(QueueTemplate {
        repo_name: repo.name.name().to_string(),
        repo_url: format!("https://github.com/{}", repo.name),
        tree_state: repo.tree_state,
        stats: PullRequestStats {
            total_count: prs.len(),
            approved_count,
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
    pub merge_queue_tx: mpsc::Sender<MergeQueueEvent>,
    pub mergeable_queue_tx: MergeableQueueSender,
    pub bors_process: Pin<Box<dyn Future<Output = ()> + Send>>,
}

/// Creates a future with a Bors process that continuously receives webhook events and reacts to
/// them.
pub fn create_bors_process(
    ctx: BorsContext,
    gh_client: Octocrab,
    team_api: TeamApiClient,
) -> BorsProcess {
    let (repository_tx, repository_rx) = mpsc::channel::<BorsRepositoryEvent>(1024);
    let (global_tx, global_rx) = mpsc::channel::<BorsGlobalEvent>(1024);
    let (merge_queue_tx, merge_queue_rx) = mpsc::channel::<()>(128);
    let (mergeable_queue_tx, mergeable_queue_rx) = create_mergeable_queue();

    let mq_tx = mergeable_queue_tx.clone();
    let merge_queue_tx_clone = merge_queue_tx.clone();

    let service = async move {
        let ctx = Arc::new(ctx);

        // In tests, we shutdown these futures by dropping the channel sender,
        // In that case, we need to wait until both of these futures resolve,
        // to make sure that they are able to handle all the events in the queue
        // before finishing.
        #[cfg(test)]
        {
            tokio::join!(
                consume_repository_events(
                    ctx.clone(),
                    repository_rx,
                    merge_queue_tx,
                    mq_tx.clone()
                ),
                consume_global_events(ctx.clone(), global_rx, mq_tx, gh_client, team_api),
                consume_merge_queue(ctx.clone(), merge_queue_rx),
                consume_mergeable_queue(ctx, mergeable_queue_rx)
            );
        }
        // In real execution, the bot runs forever. If there is something that finishes
        // the futures early, it's essentially a bug.
        // FIXME: maybe we could just use join for both versions of the code.
        #[cfg(not(test))]
        {
            tokio::select! {
                _ = consume_repository_events(ctx.clone(), repository_rx, merge_queue_tx, mq_tx.clone()) => {
                    tracing::error!("Repository event handling process has ended");
                }
                _ = consume_global_events(ctx.clone(), global_rx, mq_tx, gh_client, team_api) => {
                    tracing::error!("Global event handling process has ended");
                }
                _ = consume_merge_queue(ctx.clone(), merge_queue_rx) => {
                    tracing::error!("Merge queue handling process has ended");
                }
                _ = consume_mergeable_queue(ctx, mergeable_queue_rx) => {
                    tracing::error!("Mergeable queue handling process has ended")
                }
            }
        }
    };

    BorsProcess {
        repository_tx,
        global_tx,
        merge_queue_tx: merge_queue_tx_clone,
        mergeable_queue_tx,
        bors_process: Box::pin(service),
    }
}

async fn consume_repository_events(
    ctx: Arc<BorsContext>,
    mut repository_rx: mpsc::Receiver<BorsRepositoryEvent>,
    merge_queue_tx: mpsc::Sender<MergeQueueEvent>,
    mergeable_queue_tx: MergeableQueueSender,
) {
    while let Some(event) = repository_rx.recv().await {
        let ctx = ctx.clone();
        let mergeable_queue_tx = mergeable_queue_tx.clone();
        let merge_queue_tx = merge_queue_tx.clone();

        let span = tracing::info_span!("RepositoryEvent");
        tracing::debug!("Received repository event: {event:#?}");
        if let Err(error) =
            handle_bors_repository_event(event, ctx, merge_queue_tx, mergeable_queue_tx)
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
    mergeable_queue_tx: MergeableQueueSender,
    gh_client: Octocrab,
    team_api: TeamApiClient,
) {
    while let Some(event) = global_rx.recv().await {
        let ctx = ctx.clone();
        let mergeable_queue_tx = mergeable_queue_tx.clone();

        let span = tracing::info_span!("GlobalEvent");
        tracing::debug!("Received global event: {event:#?}");
        if let Err(error) =
            handle_bors_global_event(event, ctx, &gh_client, &team_api, mergeable_queue_tx)
                .instrument(span.clone())
                .await
        {
            handle_root_error(span, error);
        }
    }
}

async fn consume_merge_queue(
    ctx: Arc<BorsContext>,
    mut merge_queue_rx: mpsc::Receiver<MergeQueueEvent>,
) {
    while merge_queue_rx.recv().await.is_some() {
        // Drain any extras that have arrived.
        while let Ok(()) = merge_queue_rx.try_recv() {
            // We can do this as we know the state of the queue has changed
            // and this single run should capture those changes.
        }

        let ctx = ctx.clone();

        let span = tracing::info_span!("MergeQueue");
        tracing::debug!("Processing merge queue");
        if let Err(error) = handle_merge_queue(ctx).instrument(span.clone()).await {
            handle_root_error(span, error);
        }
    }
}

async fn consume_mergeable_queue(
    ctx: Arc<BorsContext>,
    mergeable_queue_rx: MergeableQueueReceiver,
) {
    while let Some((mq_item, mq_tx)) = mergeable_queue_rx.dequeue().await {
        let ctx = ctx.clone();

        let span = tracing::info_span!("MergeableQueue");
        tracing::debug!("Received mergeable queue item: {}", mq_item.pull_request);
        if let Err(error) = handle_mergeable_queue_item(ctx, mq_tx, mq_item)
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
