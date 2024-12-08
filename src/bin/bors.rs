use std::collections::HashMap;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bors::{
    create_app, create_bors_process, create_github_client, create_github_client_from_access_token,
    load_repositories, BorsContextBuilder, BorsGlobalEvent, CommandParser, PgDbClient, ServerState,
    TeamApiClient, WebhookSecret,
};
use clap::Parser;
use sqlx::postgres::PgConnectOptions;
use sqlx::{ConnectOptions, PgPool};
use tracing::log::LevelFilter;
use tracing_subscriber::filter::EnvFilter;

/// How often should the bot check DB state, e.g. for handling timeouts.
const PERIODIC_REFRESH: Duration = Duration::from_secs(120);

const GITHUB_API_URL: &str = "https://api.github.com";

#[derive(Parser)]
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
    #[arg(long, env = "DATABASE_URL")]
    db: String,

    /// Prefix used for bot commands in PR comments.
    #[arg(long, env = "CMD_PREFIX", default_value = "@bors")]
    cmd_prefix: String,

    /// Prefix used for bot commands in PR comments.
    #[arg(long, env = "CI_ACCESS_TOKEN")]
    ci_access_token: Option<String>,
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

async fn initialize_db(connection_string: &str) -> anyhow::Result<PgDbClient> {
    let mut opts: PgConnectOptions = connection_string.parse()?;
    opts = opts.log_statements(LevelFilter::Trace);
    let db = PgPool::connect_with(opts)
        .await
        .context("Cannot connect to database")?;

    sqlx::migrate!()
        .run(&db)
        .await
        .context("Cannot run database migrations")?;

    Ok(PgDbClient::new(db))
}

fn try_main(opts: Opts) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Cannot build tokio runtime")?;

    let db = runtime
        .block_on(initialize_db(&opts.db))
        .context("Cannot initialize database")?;
    let team_api_client = TeamApiClient::default();
    let gh_app_client = runtime.block_on(async {
        create_github_client(
            opts.app_id.into(),
            GITHUB_API_URL.to_string(),
            opts.private_key.into(),
        )
    })?;
    let ci_client = match opts.ci_access_token {
        Some(access_token) => {
            let client = runtime.block_on(async {
                tracing::warn!("creating client ci");
                create_github_client_from_access_token(
                    GITHUB_API_URL.to_string(),
                    access_token.into(),
                )
            })?;
            Some(client)
        }
        None => None,
    };
    let loaded_repos = runtime.block_on(async {
        let repos = load_repositories(&gh_app_client, &team_api_client).await?;
        Ok::<_, anyhow::Error>(repos)
    })?;

    let mut repos = HashMap::default();
    for (name, repo) in loaded_repos {
        let repo = match repo {
            Ok(repo) => {
                tracing::info!("Loaded repository {name}");
                repo
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "Failed to load repository {name}: {error:?}"
                ));
            }
        };
        repos.insert(name, Arc::new(repo));
    }

    let ctx = BorsContextBuilder::default()
        .parser(CommandParser::new(opts.cmd_prefix))
        .db(Arc::new(db))
        .repositories(repos)
        .gh_app_client(gh_app_client)
        .ci_client(ci_client)
        .team_api_client(team_api_client)
        .build()
        .unwrap();
    let (repository_tx, global_tx, bors_process) = create_bors_process(ctx);

    let refresh_tx = global_tx.clone();
    let refresh_process = async move {
        loop {
            tokio::time::sleep(PERIODIC_REFRESH).await;
            refresh_tx.send(BorsGlobalEvent::Refresh).await?;
        }
    };

    let state = ServerState::new(
        repository_tx,
        global_tx,
        WebhookSecret::new(opts.webhook_secret),
    );
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
