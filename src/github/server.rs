use crate::bors::event::BorsEvent;
use crate::bors::{handle_bors_event, BorsContext, BorsState};
use crate::github::api::GithubAppState;
use crate::github::webhook::GitHubWebhook;
use crate::github::webhook::WebhookSecret;
use crate::utils::logging::LogError;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::Instrument;

/// Shared server state for all axum handlers.
pub struct ServerState {
    webhook_sender: WebhookSender,
    webhook_secret: WebhookSecret,
}

impl ServerState {
    pub fn new(webhook_sender: WebhookSender, webhook_secret: WebhookSecret) -> Self {
        Self {
            webhook_sender,
            webhook_secret,
        }
    }

    pub fn get_webhook_secret(&self) -> &WebhookSecret {
        &self.webhook_secret
    }
}

pub type ServerStateRef = Arc<ServerState>;

/// Axum handler that receives a webhook and sends it to a webhook channel.
pub async fn github_webhook_handler(
    State(state): State<ServerStateRef>,
    GitHubWebhook(event): GitHubWebhook,
) -> impl IntoResponse {
    match state.webhook_sender.send(event).await {
        Ok(_) => (StatusCode::OK, ""),
        Err(err) => {
            tracing::error!("Could not send webhook event: {err:?}");
            (StatusCode::INTERNAL_SERVER_ERROR, "")
        }
    }
}

type WebhookSender = mpsc::Sender<BorsEvent>;

/// Creates a future with a Bors process that continuously receives webhook events and reacts to
/// them.
pub fn create_bors_process(
    state: GithubAppState,
    ctx: BorsContext,
) -> (WebhookSender, impl Future<Output = ()>) {
    let (tx, mut rx) = mpsc::channel::<BorsEvent>(1024);

    let service = async move {
        let state: Arc<dyn BorsState<_>> = Arc::new(state);
        let ctx = Arc::new(ctx);
        while let Some(event) = rx.recv().await {
            tracing::trace!("Received event: {event:#?}");
            let state = state.clone();
            let ctx = ctx.clone();

            tokio::spawn(async move {
                let span = tracing::info_span!("Event");
                if let Err(error) = handle_bors_event(event, state, ctx)
                    .instrument(span.clone())
                    .await
                {
                    span.log_error(error);
                }
            });
        }
    };
    (tx, service)
}
