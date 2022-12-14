use axum::body::{Bytes, HttpBody};
use axum::extract::FromRequest;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use axum::{async_trait, RequestExt};
use hmac::{Hmac, Mac};
use octocrab::models::events::payload::IssueCommentEventPayload;
use octocrab::models::Repository;
use sha2::Sha256;
use std::fmt::Debug;

use crate::github::server::ServerStateRef;
use crate::github::{GithubRepoName, WebhookSecret};

/// This struct is used to extract the repository and user from a GitHub webhook event.
/// The wrapper exists because octocrab doesn't expose/parse the repository field.
#[derive(serde::Deserialize, Debug)]
pub struct WebhookEventRepository {
    pub repository: Repository,
}

#[derive(Debug)]
pub struct WebhookCommentEvent {
    pub repository: GithubRepoName,
    pub payload: IssueCommentEventPayload,
}

#[derive(Debug)]
pub enum WebhookEvent {
    Comment(WebhookCommentEvent),
    InstallationsChanged,
}

/// axum extractor for GitHub webhook events.
#[derive(Debug)]
pub struct GitHubWebhook(pub WebhookEvent);

/// Extracts a webhook event from a HTTP request.
#[async_trait]
impl<B> FromRequest<ServerStateRef, B> for GitHubWebhook
where
    B: HttpBody + Send + Debug + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    type Rejection = StatusCode;

    async fn from_request(
        request: Request<B>,
        state: &ServerStateRef,
    ) -> Result<Self, Self::Rejection> {
        let (parts, body) = request
            .with_limited_body()
            .expect("There should be a body size limit")
            .into_parts();

        // Eagerly load body
        let body: Bytes = hyper::body::to_bytes(body).await.map_err(|error| {
            log::error!("Parsing webhook body failed: {error:?}");
            StatusCode::BAD_REQUEST
        })?;

        // Verify that the request is valid
        if !verify_gh_signature(&parts.headers, &body, state.get_webhook_secret()) {
            log::error!("Webhook request failed, could not authenticate webhook");
            return Err(StatusCode::BAD_REQUEST);
        }

        // Parse webhook content
        match parse_webhook_event(parts, &body) {
            Ok(Some(event)) => Ok(GitHubWebhook(event)),
            Ok(None) => Err(StatusCode::OK),
            Err(error) => {
                log::error!("Cannot parse webhook event: {error:?}");
                Err(StatusCode::BAD_REQUEST)
            }
        }
    }
}

fn parse_webhook_event(request: Parts, body: &[u8]) -> anyhow::Result<Option<WebhookEvent>> {
    let Some(event_type) = request.headers.get("x-github-event") else {
         return Err(anyhow::anyhow!("x-github-event header not found"));
    };

    match event_type.as_bytes() {
        b"issue_comment" => {
            let repository: WebhookEventRepository = serde_json::from_slice(body)?;
            let repo_name = repository.repository.name;
            let Some(repo_owner) = repository
                .repository
                .owner
                .map(|u| u.login) else {
                return Err(anyhow::anyhow!("Owner for repo {repo_name} is missing"));
            };
            let repository_name = GithubRepoName::new(&repo_owner, &repo_name);

            let event: IssueCommentEventPayload = serde_json::from_slice(body)?;
            Ok(Some(WebhookEvent::Comment(WebhookCommentEvent {
                repository: repository_name,
                payload: event,
            })))
        }
        b"installation_repositories" | b"installation" => {
            Ok(Some(WebhookEvent::InstallationsChanged))
        }
        _ => {
            log::debug!("Ignoring unknown event type {:?}", event_type.to_str());
            Ok(None)
        }
    }
}

type HmacSha256 = Hmac<Sha256>;

/// Verifies that the request is properly signed by GitHub with SHA-256 and the passed `secret`.
fn verify_gh_signature(
    headers: &HeaderMap<HeaderValue>,
    body: &[u8],
    secret: &WebhookSecret,
) -> bool {
    let Some(signature) = headers.get("x-hub-signature-256").map(|v| v.as_bytes()) else {
        return false;
    };
    let Some(signature) = signature.get(b"sha256=".len()..).and_then(|v| hex::decode(v).ok()) else {
        return false;
    };

    let mut mac =
        HmacSha256::new_from_slice(secret.expose().as_bytes()).expect("Cannot create HMAC key");
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}
