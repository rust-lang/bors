use std::sync::Arc;

use axum::Router;
use hmac::Mac;
use http::{HeaderValue, Method, Request};
use octocrab::{Octocrab, OctocrabBuilder};
use tower::Service;
use wiremock::MockServer;

use crate::{
    create_app, create_bors_process, github::webhook::HmacSha256, BorsContext, CommandParser,
    ServerState, TeamApiClient, WebhookSecret,
};

use super::{
    database::create_test_db,
    mocks::{
        self,
        issue::{InMemoryIssue, IssueCommentEventPayload},
    },
};

const WEBHOOK_SECRET: &str = "ABCDEF";

pub(crate) struct BorsTester {
    in_memory_mock: InMemoryIssue,
    app: Router,
}

impl BorsTester {
    pub(crate) async fn new() -> Self {
        let mock_server = MockServer::start().await;
        let in_memory_mock = mocks::setup_mock(&mock_server).await;

        let db = create_test_db().await;
        let client = create_test_github_client(&mock_server);
        let team_api_client = TeamApiClient::new(Some(mock_server.uri().as_str()));

        let ctx = BorsContext::new(
            CommandParser::new("@bors".to_string()),
            Arc::new(db),
            Arc::new(client),
            team_api_client,
        )
        .await
        .unwrap();

        let (repository_tx, global_tx, bors_process) = create_bors_process(ctx);

        let state = ServerState::new(
            repository_tx,
            global_tx,
            WebhookSecret::new(WEBHOOK_SECRET.to_string()),
        );
        let app = create_app(state);
        tokio::spawn(bors_process);
        Self {
            app,
            in_memory_mock,
        }
    }

    pub(crate) async fn comment(&mut self, body: &str) {
        let comment = IssueCommentEventPayload::new(body);
        let webhook_request = self
            .create_webhook_request(
                serde_json::to_string(&comment).unwrap().as_str(),
                "issue_comment",
            )
            .await;
        self.send_webhook(webhook_request).await;
    }

    pub(crate) async fn check_comments(&self, expected_comments: &[&str]) {
        assert!(self.in_memory_mock.check_comments(expected_comments).await);
    }

    async fn create_webhook_request(&self, body: &str, event: &str) -> Request<axum::body::Body> {
        let body_length = body.len();
        let secret = WEBHOOK_SECRET.to_string();
        let new_from_slice = HmacSha256::new_from_slice(secret.as_bytes());
        let mut mac = new_from_slice.expect("Cannot create HMAC key");
        mac.update(body.as_bytes());
        let hash = mac.finalize().into_bytes();
        let hash = hex::encode(hash);
        let signature = format!("sha256={hash}");

        Request::builder()
            .uri("/github")
            .method(Method::POST)
            .header("content-type", HeaderValue::from_static("application-json"))
            .header(
                "content-length",
                HeaderValue::from_str(&body_length.to_string()).unwrap(),
            )
            .header("x-github-event", HeaderValue::from_str(event).unwrap())
            .header(
                "x-hub-signature-256",
                HeaderValue::from_str(&signature).unwrap(),
            )
            .body(axum::body::Body::from(body.to_owned()))
            .unwrap()
    }

    async fn send_webhook(&mut self, request: Request<axum::body::Body>) {
        let service =
            <axum::Router as tower::ServiceExt<Request<axum::body::Body>>>::ready(&mut self.app)
                .await
                .unwrap();
        service.call(request).await.unwrap();
    }
}

fn create_test_github_client(mock_server: &MockServer) -> Octocrab {
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(mocks::PRIVATE_KEY.as_bytes()).unwrap();
    OctocrabBuilder::new()
        .base_uri(mock_server.uri())
        .unwrap()
        .app(6.into(), key)
        .build()
        .unwrap()
}
