use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use axum::Router;
use serde::Serialize;
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tower::Service;

use crate::github::api::load_repositories;
use crate::tests::database::MockedDBClient;
use crate::tests::mocks::comment::{Comment, GitHubIssueCommentEventPayload};
use crate::tests::mocks::webhook::{create_webhook_request, TEST_WEBHOOK_SECRET};
use crate::tests::mocks::{ExternalHttpMock, World};
use crate::{
    create_app, create_bors_process, BorsContext, CommandParser, ServerState, WebhookSecret,
};

pub struct BorsBuilder {
    world: World,
    pool: PgPool,
}

impl BorsBuilder {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            world: Default::default(),
        }
    }

    pub fn world(self, world: World) -> Self {
        Self { world, ..self }
    }

    /// This closure is used to ensure that the test has to return `BorsTester`
    /// to us, so that we can call `finish()` on it. Without that call, we couldn't
    /// ensure that some async task within the bors process hasn't crashed.
    pub async fn run_test<
        F: FnOnce(BorsTester) -> Fut,
        Fut: Future<Output = anyhow::Result<BorsTester>>,
    >(
        self,
        f: F,
    ) {
        let tester = BorsTester::new(self.pool, self.world).await;
        let tester = f(tester).await.unwrap();
        tester.finish().await
    }
}

/// Simple end-to-end test entrypoint for tests that don't need to prepare any custom state.
pub async fn run_test<
    F: FnOnce(BorsTester) -> Fut,
    Fut: Future<Output = anyhow::Result<BorsTester>>,
>(
    pool: PgPool,
    f: F,
) {
    BorsBuilder::new(pool).run_test(f).await
}

/// Represents a running bors web application and also the background
/// bors process. This structure should be used in tests to test interaction
/// with the bot.
///
/// Dropping this struct will drop `app`, which will close the
/// send channels for the bors process, which should stop its async task.
pub struct BorsTester {
    app: Router,
    http_mock: ExternalHttpMock,
    bors: JoinHandle<()>,
}

impl BorsTester {
    async fn new(pool: PgPool, world: World) -> Self {
        let mock = ExternalHttpMock::start(&world).await;
        let db = MockedDBClient::new(pool);

        let loaded_repos = load_repositories(&mock.github_client(), &mock.team_api_client())
            .await
            .unwrap();
        let mut repos = HashMap::default();
        for (name, repo) in loaded_repos {
            let repo = repo.unwrap();
            repos.insert(name, Arc::new(repo));
        }

        let ctx = BorsContext::new(CommandParser::new("@bors".to_string()), Arc::new(db), repos);

        let (repository_tx, global_tx, bors_process) =
            create_bors_process(ctx, mock.github_client(), mock.team_api_client());

        let state = ServerState::new(
            repository_tx,
            global_tx,
            WebhookSecret::new(TEST_WEBHOOK_SECRET.to_string()),
        );
        let app = create_app(state);
        let bors = tokio::spawn(bors_process);
        Self {
            app,
            http_mock: mock,
            bors,
        }
    }

    pub async fn post_comment(&mut self, content: &str) {
        self.webhook_comment(Comment::new(content)).await;
    }

    async fn webhook_comment(&mut self, comment: Comment) {
        self.send_webhook(
            "issue_comment",
            GitHubIssueCommentEventPayload::from(comment),
        )
        .await;
    }

    async fn send_webhook<S: Serialize>(&mut self, event: &str, content: S) {
        let webhook = create_webhook_request(event, content);
        let response = self
            .app
            .call(webhook)
            .await
            .expect("Cannot send webhook request");
        let status = response.status();
        if !status.is_success() {
            panic!("Wrong status code {status}");
        }
        let body_text = String::from_utf8(
            axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        tracing::debug!("Received webhook with status {status} and response body `{body_text}`");
    }

    pub async fn finish(self) {
        drop(self.app);
        self.bors.await.unwrap();
    }
}
