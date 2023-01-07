use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::routing::post;
use axum::Router;
use clap::Parser;
use tower::limit::ConcurrencyLimitLayer;

use bors::github::api::GithubAppClient;
use bors::github::server::{github_webhook_handler, ServerState};
use bors::github::service::github_webhook_process;
use bors::github::WebhookSecret;

#[derive(clap::Parser)]
struct Opts {
    /// Secret used to authenticate webhooks.
    #[arg(long, env = "WEBHOOK_SECRET")]
    webhook_secret: String,

    /// Github App ID.
    #[arg(long, env = "APP_ID")]
    app_id: u64,

    /// Private key used to authenticate as a Github App.
    #[arg(long, env = "PRIVATE_KEY")]
    private_key: String,
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

    let access = runtime.block_on(GithubAppClient::load(
        opts.app_id.into(),
        opts.private_key.into_bytes().into(),
    ))?;
    let (tx, gh_process) = github_webhook_process(access);

    let state = ServerState::new(tx, WebhookSecret::new(opts.webhook_secret));
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
