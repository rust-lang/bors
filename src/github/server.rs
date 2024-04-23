use crate::bors::event::BorsEvent;
use crate::bors::{handle_bors_global_event, handle_bors_repository_event, BorsContext};
use crate::github::webhook::GitHubWebhook;
use crate::github::webhook::WebhookSecret;
use crate::utils::logging::LogError;
use crate::{BorsGlobalEvent, BorsRepositoryEvent};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;
use tower::limit::ConcurrencyLimitLayer;
use tracing::Instrument;

use super::api::client::GithubRepositoryClient;

/// Shared server state for all axum handlers.
pub struct ServerState {
    repository_event_queue: mpsc::Sender<BorsRepositoryEvent>,
    global_event_queue: mpsc::Sender<BorsGlobalEvent>,
    webhook_secret: WebhookSecret,
}

impl ServerState {
    pub fn new(
        repository_event_queue: mpsc::Sender<BorsRepositoryEvent>,
        global_event_queue: mpsc::Sender<BorsGlobalEvent>,
        webhook_secret: WebhookSecret,
    ) -> Self {
        Self {
            repository_event_queue,
            global_event_queue,
            webhook_secret,
        }
    }

    pub fn get_webhook_secret(&self) -> &WebhookSecret {
        &self.webhook_secret
    }
}

pub type ServerStateRef = Arc<ServerState>;

pub fn create_app(state: ServerState) -> Router {
    Router::new()
        .route("/github", post(github_webhook_handler))
        .layer(ConcurrencyLimitLayer::new(100))
        .with_state(Arc::new(state))
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

/// Creates a future with a Bors process that continuously receives webhook events and reacts to
/// them.
pub fn create_bors_process(
    ctx: BorsContext<GithubRepositoryClient>,
) -> (
    mpsc::Sender<BorsRepositoryEvent>,
    mpsc::Sender<BorsGlobalEvent>,
    impl Future<Output = ()>,
) {
    let (repository_tx, repository_rx) = mpsc::channel::<BorsRepositoryEvent>(1024);
    let (global_tx, global_rx) = mpsc::channel::<BorsGlobalEvent>(1024);

    let service = async move {
        let ctx = Arc::new(ctx);
        tokio::select! {
            _ = consume_repository_events(ctx.clone(), repository_rx) => {
                tracing::warn!("Repository event handling process has ended");
            }
            _ = consume_global_events(ctx.clone(), global_rx) => {
                tracing::warn!("Global event handling process has ended");
            }
        }
    };
    (repository_tx, global_tx, service)
}

async fn consume_repository_events(
    ctx: Arc<BorsContext<GithubRepositoryClient>>,
    mut repository_rx: mpsc::Receiver<BorsRepositoryEvent>,
) {
    while let Some(event) = repository_rx.recv().await {
        let ctx = ctx.clone();

        let span = tracing::info_span!("RepositoryEvent");
        tracing::debug!("Received repository event: {event:#?}");
        if let Err(error) = handle_bors_repository_event(event, ctx)
            .instrument(span.clone())
            .await
        {
            span.log_error(error);
        }
    }
}

async fn consume_global_events(
    ctx: Arc<BorsContext<GithubRepositoryClient>>,
    mut global_rx: mpsc::Receiver<BorsGlobalEvent>,
) {
    while let Some(event) = global_rx.recv().await {
        let ctx = ctx.clone();

        let span = tracing::info_span!("GlobalEvent");
        tracing::debug!("Received global event: {event:#?}");
        if let Err(error) = handle_bors_global_event(event, ctx)
            .instrument(span.clone())
            .await
        {
            span.log_error(error);
        }
    }
}
