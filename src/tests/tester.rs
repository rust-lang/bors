use axum::Router;
use sqlx::PgPool;

const WEBHOOK_SECRET: &str = "ABCDEF";

pub(crate) struct BorsTester {
    #[allow(dead_code)]
    app: Router,
}

impl BorsTester {
    pub(crate) async fn new(pool: PgPool) -> Self {
        // let gh_mock_server = RunningMock::start().await;
        // let team_api_mock_server = TeamApiMockServer::start().await;

        // let client = create_test_github_client(&gh_mock_server);
        // let db = MockedDBClient::new(pool);

        // let ctx = BorsContext::new(
        //     CommandParser::new("@bors".to_string()),
        //     Arc::new(db),
        //     Default::default(),
        // );

        // let (repository_tx, global_tx, bors_process) = create_bors_process(ctx, todo!(), todo!());

        // let state = ServerState::new(
        //     repository_tx,
        //     global_tx,
        //     WebhookSecret::new(WEBHOOK_SECRET.to_string()),
        // );
        // let app = create_app(state);
        // tokio::spawn(bors_process);
        // Self { app }
        todo!()
    }
}
