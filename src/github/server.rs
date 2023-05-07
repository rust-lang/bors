use crate::bors::event::BorsEvent;
use crate::bors::handle_bors_event;
use crate::github::api::GithubAppState;
use crate::github::webhook::GitHubWebhook;
use crate::github::webhook::WebhookSecret;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;

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
            log::error!("Could not handle webhook event: {err:?}");
            (StatusCode::INTERNAL_SERVER_ERROR, "")
        }
    }
}

type WebhookSender = mpsc::Sender<BorsEvent>;

/// Creates a future with a Bors process that continuously receives webhook events and reacts to
/// them.
pub fn create_bors_process(mut state: GithubAppState) -> (WebhookSender, impl Future<Output = ()>) {
    let (tx, mut rx) = mpsc::channel::<BorsEvent>(1024);

    let service = async move {
        while let Some(event) = rx.recv().await {
            log::trace!("Received event: {event:#?}");
            if let Err(error) = handle_bors_event(event, &mut state).await {
                log::error!("Error while handling event: {error:?}");
            }
        }
    };
    (tx, service)
}
