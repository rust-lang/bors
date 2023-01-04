use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use clap::Parser;
use tower::limit::ConcurrencyLimitLayer;

use bors::github::api::RepositoryAccess;
use bors::github::service::{github_webhook_process, WebhookSender};
use bors::github::webhook::GitHubWebhook;
use bors::github::WebhookSecret;

#[derive(clap::Parser)]
struct Opts {
    /// Secret used to authenticate webhooks.
    #[arg(long, env = "WEBHOOK_SECRET")]
    webhook_secret: String,
    /// Github App ID.
    #[arg(long, env = "APP_ID")]
    app_id: u64,
    /// Path to a private key used to authenticate as a Github App.
    #[arg(long, env = "PRIVATE_KEY_PATH")]
    private_key_path: PathBuf,
}

struct ServerState {
    sender: WebhookSender,
    webhook_secret: WebhookSecret,
}

impl AsRef<WebhookSecret> for ServerState {
    fn as_ref(&self) -> &WebhookSecret {
        &self.webhook_secret
    }
}

type ServerStateRef = Arc<ServerState>;

async fn github_webhook_handler(
    State(state): State<ServerStateRef>,
    GitHubWebhook(event): GitHubWebhook,
) -> impl IntoResponse {
    match state.sender.send(event).await {
        Ok(_) => (StatusCode::OK, ""),
        Err(err) => {
            log::error!("Could not handle webhook event: {err:?}");
            (StatusCode::INTERNAL_SERVER_ERROR, "")
        }
    }
}

async fn server(state: ServerState) -> anyhow::Result<()> {
    let state = Arc::new(state);

    let app = Router::new()
        .route("/github", post(github_webhook_handler))
        .layer(ConcurrencyLimitLayer::new(100))
        .with_state(state);
    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await?;
    Ok(())
}

fn try_main(opts: Opts) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Cannot build tokio runtime")?;

    let private_key = std::fs::read(opts.private_key_path).context("Could not read private key")?;
    let access = runtime.block_on(RepositoryAccess::load_repositories(
        opts.app_id.into(),
        private_key.into(),
    ))?;
    let (tx, gh_process) = github_webhook_process(access);

    let state = ServerState {
        sender: tx,
        webhook_secret: WebhookSecret::new(opts.webhook_secret),
    };
    let server_process = server(state);

    runtime.block_on(async move {
        tokio::select! {
            () = gh_process => {
                log::warn!("Github webhook process has ended");
                Ok(())
            },
            res = server_process => {
                log::warn!("Server has ended: {res:?}");
                res
            }
        }
    })?;

    Ok(())
}

fn main() {
    env_logger::init();

    let opts = Opts::parse();
    if let Err(error) = try_main(opts) {
        eprintln!("Error: {error:?}");
        std::process::exit(1);
    }
}
