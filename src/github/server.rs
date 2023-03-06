use crate::github::process::WebhookSender;
use crate::github::webhook::GitHubWebhook;
use crate::github::WebhookSecret;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use std::sync::Arc;

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
