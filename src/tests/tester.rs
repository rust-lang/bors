use std::sync::Arc;

use axum::Router;
use octocrab::{Octocrab, OctocrabBuilder};
use sqlx::PgPool;

use crate::{
    create_app, create_bors_process, github::api::client::GithubRepositoryClient,
    permissions::TeamApiClient, BorsContext, CommandParser, ServerState, WebhookSecret,
};

use super::{
    database::MockedDBClient,
    mocks::{GithubMockServer, GITHUB_MOCK_PRIVATE_KEY},
    permission::TeamApiMockServer,
};

const WEBHOOK_SECRET: &str = "ABCDEF";

pub(crate) struct BorsTester {
    #[allow(dead_code)]
    app: Router,
}

impl BorsTester {
    pub(crate) async fn new(pool: PgPool) -> Self {
        let ctx = create_test_context(pool).await;
        let (repository_tx, global_tx, bors_process) = create_bors_process(ctx);

        let state = ServerState::new(
            repository_tx,
            global_tx,
            WebhookSecret::new(WEBHOOK_SECRET.to_string()),
        );
        tokio::spawn(bors_process);
        let app = create_app(state);
        Self { app }
    }
}

fn create_test_github_client(mock_server: &GithubMockServer) -> Octocrab {
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(GITHUB_MOCK_PRIVATE_KEY.as_bytes()).unwrap();
    OctocrabBuilder::new()
        .base_uri(mock_server.uri())
        .unwrap()
        .app(6.into(), key)
        .build()
        .unwrap()
}

async fn create_test_context(pool: PgPool) -> BorsContext<GithubRepositoryClient> {
    let gh_mock_server = GithubMockServer::start().await;
    let team_api_mock_server = TeamApiMockServer::start().await;

    let client = create_test_github_client(&gh_mock_server);
    let db = MockedDBClient::new(pool);

    BorsContext::new(
        CommandParser::new("@bors".to_string()),
        Arc::new(db),
        Arc::new(client),
        TeamApiClient::new(team_api_mock_server.uri()),
    )
    .await
    .unwrap()
}

#[sqlx::test]
async fn test_load_repositories(pool: PgPool) {
    let ctx = create_test_context(pool).await;
    let repositories = ctx.repositories.read().unwrap();
    assert_eq!(repositories.len(), 1);
}
