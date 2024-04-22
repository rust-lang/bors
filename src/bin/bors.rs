use std::io::IsTerminal;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bors::{
    create_app, create_bors_process, BorsContext, BorsEvent, BorsGlobalEvent, CommandParser,
    GithubAppState, SeaORMClient, ServerState, WebhookSecret,
};
use clap::Parser;
use sea_orm::Database;
use tracing_subscriber::EnvFilter;

use migration::{Migrator, MigratorTrait};

/// How often should the bot check DB state, e.g. for handling timeouts.
const PERIODIC_REFRESH: Duration = Duration::from_secs(120);

#[derive(clap::Parser)]
struct Opts {
    /// Github App ID.
    #[arg(long, env = "APP_ID")]
    app_id: u64,

    /// Private key used to authenticate as a Github App.
    #[arg(long, env = "PRIVATE_KEY")]
    private_key: String,

    /// Secret used to authenticate webhooks.
    #[arg(long, env = "WEBHOOK_SECRET")]
    webhook_secret: String,

    /// Database connection string.
    #[arg(long, env = "DATABASE")]
    db: String,

    /// Prefix used for bot commands in PR comments.
    #[arg(long, env = "CMD_PREFIX", default_value = "@bors")]
    cmd_prefix: String,
}

/// Starts a server that receives GitHub webhooks and generates events into a queue
/// that is then handled by the Bors process.
async fn webhook_server(state: ServerState) -> anyhow::Result<()> {
    let app = create_app(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("Cannot create TCP/IP server socket")?;

    tracing::info!("Listening on 0.0.0.0:{}", listener.local_addr()?.port());

    axum::serve(listener, app).await?;
    Ok(())
}

async fn initialize_db(connection_string: &str) -> anyhow::Result<SeaORMClient> {
    let db = Database::connect(connection_string).await?;
    Migrator::up(&db, None).await?;
    Ok(SeaORMClient::new(db))
}

fn try_main(opts: Opts) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Cannot build tokio runtime")?;

    let db = runtime
        .block_on(initialize_db(&opts.db))
        .context("Cannot initialize database")?;

    let state = runtime
        .block_on(GithubAppState::load(
            opts.app_id.into(),
            opts.private_key.into_bytes().into(),
        ))
        .context("Cannot load GitHub repository state")?;
    let ctx = BorsContext::new(CommandParser::new(opts.cmd_prefix), Arc::new(db));
    let (tx, bors_process) = create_bors_process(state, ctx);

    let refresh_tx = tx.clone();
    let refresh_process = async move {
        loop {
            tokio::time::sleep(PERIODIC_REFRESH).await;
            refresh_tx
                .send(BorsEvent::Global(BorsGlobalEvent::Refresh))
                .await?;
        }
    };

    let state = ServerState::new(tx, WebhookSecret::new(opts.webhook_secret));
    let server_process = webhook_server(state);

    let fut = async move {
        tokio::select! {
            () = bors_process => {
                tracing::warn!("Bors event handling process has ended");
                Ok(())
            },
            res = refresh_process => {
                tracing::warn!("Refresh generator has ended");
                res
            }
            res = server_process => {
                tracing::warn!("GitHub webhook listener has ended: {res:?}");
                res
            }
        }
    };

    runtime.block_on(fut)?;

    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(tracing::Level::INFO.into())
                .from_env()
                .expect("Cannot load RUST_LOG"),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let opts = Opts::parse();
    if let Err(error) = try_main(opts) {
        tracing::error!("Error: {error:?}");
        std::process::exit(1);
    }
}
