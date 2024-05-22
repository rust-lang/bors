use std::sync::Arc;

use axum::Router;
use octocrab::{Octocrab, OctocrabBuilder};
use sqlx::PgPool;

use crate::{
    create_app, create_bors_process, BorsContext, CommandParser, ServerState, WebhookSecret,
};

use super::{
    database::MockedDBClient,
    mocks::{GithubMockServer, PRIVATE_KEY},
};

const WEBHOOK_SECRET: &str = "ABCDEF";

pub(crate) struct BorsTester {
    #[allow(dead_code)]
    app: Router,
}

impl BorsTester {
    pub(crate) async fn new(pool: PgPool) -> Self {
        let mock_server = GithubMockServer::start().await;

        let client = create_test_github_client(&mock_server);
        let db = MockedDBClient::new(pool);

        let ctx = BorsContext::new(
            CommandParser::new("@bors".to_string()),
            Arc::new(db),
            Arc::new(client),
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
        Self { app }
    }
}

fn create_test_github_client(mock_server: &GithubMockServer) -> Octocrab {
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(PRIVATE_KEY.as_bytes()).unwrap();
    OctocrabBuilder::new()
        .base_uri(mock_server.uri())
        .unwrap()
        .app(6.into(), key)
        .build()
        .unwrap()
}
