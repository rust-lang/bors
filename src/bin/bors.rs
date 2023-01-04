use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use clap::Parser;
use tower::limit::ConcurrencyLimitLayer;

use bors::config::Config;
use bors::github::webhook::GitHubWebhook;

#[derive(clap::Parser)]
struct Opts {
    /// Secret used to authenticate webhooks.
    #[arg(long, env = "WEBHOOK_SECRET")]
    webhook_secret: String,
}

async fn github_webhook(GitHubWebhook(event): GitHubWebhook) -> impl IntoResponse {
    println!("Received GitHub webhook: {:?}", event);
    (StatusCode::OK, "")
}

async fn server(config: Config) -> anyhow::Result<()> {
    let state = Arc::new(config);

    let app = Router::new()
        .route("/github", post(github_webhook))
        .layer(ConcurrencyLimitLayer::new(100))
        .with_state(state);
    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await?;
    Ok(())
}

fn try_main(opts: Opts) -> anyhow::Result<()> {
    let config = Config::new(opts.webhook_secret);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Cannot build tokio runtime")?;

    let server_fut = runtime.spawn(server(config));
    runtime.block_on(server_fut)??;

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
