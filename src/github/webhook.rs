use std::fmt::Debug;

use axum::body::{Bytes, HttpBody};
use axum::extract::FromRequest;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use axum::{async_trait, RequestExt};
use hmac::{Hmac, Mac};
use octocrab::models::events::payload::IssueCommentEventPayload;
use octocrab::models::Repository;
use sha2::Sha256;

use crate::github::server::ServerStateRef;
use crate::github::{GithubRepoName, GithubUser, WebhookSecret};

#[derive(Debug, PartialEq)]
pub struct PullRequestComment {
    pub repository: GithubRepoName,
    pub author: GithubUser,
    pub pr_number: u64,
    pub text: String,
}

#[derive(Debug, PartialEq)]
pub enum WebhookEvent {
    Comment(PullRequestComment),
    InstallationsChanged,
}

/// This struct is used to extract the repository and user from a GitHub webhook event.
/// The wrapper exists because octocrab doesn't expose/parse the repository field.
#[derive(serde::Deserialize, Debug)]
pub struct WebhookEventRepository {
    pub repository: Repository,
}

/// axum extractor for GitHub webhook events.
#[derive(Debug, PartialEq)]
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
            let comment = parse_pr_comment(repository_name, event).map(WebhookEvent::Comment);
            Ok(comment)
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

fn parse_pr_comment(
    repo: GithubRepoName,
    payload: IssueCommentEventPayload,
) -> Option<PullRequestComment> {
    // We only care about pull request comments
    if payload.issue.pull_request.is_none() {
        log::debug!("Ignoring event {payload:?} because it does not belong to a pull request");
        return None;
    }

    let user = GithubUser {
        username: payload.comment.user.login,
        html_url: payload.comment.user.html_url,
    };

    Some(PullRequestComment {
        repository: repo,
        author: user,
        text: payload.comment.body.unwrap_or_default(),
        pr_number: payload.issue.number,
    })
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

#[cfg(test)]
mod tests {
    use axum::extract::FromRequest;
    use axum::http::{HeaderValue, Method};
    use hmac::Mac;
    use hyper::{Request, StatusCode};
    use tokio::sync::mpsc;

    use crate::github::server::{ServerState, ServerStateRef};
    use crate::github::webhook::{GitHubWebhook, HmacSha256, WebhookEvent};
    use crate::github::WebhookSecret;
    use crate::tests::io::load_test_file;

    #[tokio::test]
    async fn test_installation_suspend() {
        assert_eq!(
            check_webhook("webhook/installation-suspend.json", "installation",).await,
            Ok(GitHubWebhook(WebhookEvent::InstallationsChanged))
        );
    }

    #[tokio::test]
    async fn test_installation_unsuspend() {
        assert_eq!(
            check_webhook("webhook/installation-unsuspend.json", "installation",).await,
            Ok(GitHubWebhook(WebhookEvent::InstallationsChanged))
        );
    }

    #[tokio::test]
    async fn test_issue_comment() {
        insta::assert_debug_snapshot!(
            check_webhook("webhook/issue-comment.json", "issue_comment").await,
            @r###"
        Ok(
            GitHubWebhook(
                Comment(
                    PullRequestComment {
                        repository: GithubRepoName {
                            owner: "kobzol",
                            name: "bors-kindergarten",
                        },
                        author: GithubUser {
                            username: "Kobzol",
                            html_url: Url {
                                scheme: "https",
                                cannot_be_a_base: false,
                                username: "",
                                password: None,
                                host: Some(
                                    Domain(
                                        "github.com",
                                    ),
                                ),
                                port: None,
                                path: "/Kobzol",
                                query: None,
                                fragment: None,
                            },
                        },
                        pr_number: 5,
                        text: "hello bors",
                    },
                ),
            ),
        )
        "###
        );
    }

    #[tokio::test]
    async fn test_unknown_event() {
        assert_eq!(
            check_webhook("webhook/push.json", "push").await,
            Err(StatusCode::OK)
        );
    }

    async fn check_webhook(file: &str, event: &str) -> Result<GitHubWebhook, StatusCode> {
        let body = load_test_file(file);
        let body_length = body.len();

        let secret = "ABCDEF".to_string();
        let mut mac =
            HmacSha256::new_from_slice(secret.as_bytes()).expect("Cannot create HMAC key");
        mac.update(body.as_bytes());
        let hash = mac.finalize().into_bytes();
        let hash = hex::encode(hash);
        let signature = format!("sha256={hash}");

        let mut request = Request::new(body.clone());
        *request.method_mut() = Method::POST;
        let headers = request.headers_mut();
        headers.insert("content-type", HeaderValue::from_static("application-json"));
        headers.insert(
            "content-length",
            HeaderValue::from_str(&body_length.to_string()).unwrap(),
        );
        headers.insert("x-github-event", HeaderValue::from_str(event).unwrap());
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_str(&signature).unwrap(),
        );

        let (tx, _) = mpsc::channel(1024);
        let server_ref = ServerStateRef::new(ServerState::new(tx, WebhookSecret::new(secret)));
        GitHubWebhook::from_request(request, &server_ref).await
    }
}
