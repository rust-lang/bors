use std::collections::HashMap;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bors::{
    BorsContext, BorsGlobalEvent, BorsProcess, CommandParser, PgDbClient, ServerState,
    TeamApiClient, TreeState, WebhookSecret, create_app, create_bors_process, create_github_client,
    load_repositories,
};
use clap::Parser;
use sqlx::postgres::PgConnectOptions;
use sqlx::{ConnectOptions, PgPool};
use tokio::time::Interval;
use tracing::log::LevelFilter;
use tracing_subscriber::filter::EnvFilter;

/// How often should the bot refresh repository configurations from GitHub.
const CONFIG_REFRESH_INTERVAL: Duration = Duration::from_secs(120);

/// How often should the bot refresh repository permissions from the team DB.
const PERMISSIONS_REFRESH_INTERVAL: Duration = Duration::from_secs(120);

/// How often should the bot attempt to time out CI builds that ran for too long.
const CANCEL_TIMED_OUT_BUILDS_INTERVAL: Duration = Duration::from_secs(60 * 5);

/// How often should the bot reload the mergeability status of PRs?
const MERGEABILITY_STATUS_INTERVAL: Duration = Duration::from_secs(60 * 10);

/// How often should the bot synchronize PR state.
const PR_STATE_PERIODIC_REFRESH: Duration = Duration::from_secs(60 * 10);

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
    #[arg(long, env = "DATABASE_URL")]
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
    let team_api = TeamApiClient::default();
    let (client, loaded_repos) = runtime.block_on(async {
        let client = create_github_client(
            opts.app_id.into(),
            "https://api.github.com".to_string(),
            opts.private_key.into(),
        )?;
        let repos = load_repositories(&client, &team_api).await?;
        Ok::<_, anyhow::Error>((client, repos))
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

        runtime.block_on(async {
            if let Err(error) = db.insert_repo_if_not_exists(&name, TreeState::Open).await {
                tracing::warn!("Failed to insert repository {name}: {error:?}");
            }
        });

        repos.insert(name, Arc::new(repo));
    }

    let db = Arc::new(db);
    let ctx = BorsContext::new(
        CommandParser::new(opts.cmd_prefix.clone()),
        db.clone(),
        repos.clone(),
    );
    let BorsProcess {
        repository_tx,
        global_tx,
        bors_process,
        ..
    } = create_bors_process(ctx, client, team_api);

    let refresh_tx = global_tx.clone();

    fn make_interval(interval: Duration) -> Interval {
        let mut interval = tokio::time::interval(interval);
        // Do not immediately trigger the interval
        interval.reset();
        interval
    }

    let refresh_process = async move {
        let mut config_refresh = make_interval(CONFIG_REFRESH_INTERVAL);
        let mut permissions_refresh = make_interval(PERMISSIONS_REFRESH_INTERVAL);
        let mut cancel_builds_refresh = make_interval(CANCEL_TIMED_OUT_BUILDS_INTERVAL);
        let mut mergeability_status_refresh = make_interval(MERGEABILITY_STATUS_INTERVAL);
        let mut prs_interval = make_interval(PR_STATE_PERIODIC_REFRESH);
        loop {
            tokio::select! {
                _ = config_refresh.tick() => {
                    refresh_tx.send(BorsGlobalEvent::RefreshConfig).await?;
                }
                _ = permissions_refresh.tick() => {
                    refresh_tx.send(BorsGlobalEvent::RefreshPermissions).await?;
                }
                _ = cancel_builds_refresh.tick() => {
                    refresh_tx.send(BorsGlobalEvent::CancelTimedOutBuilds).await?;
                }
                _ = mergeability_status_refresh.tick() => {
                    refresh_tx.send(BorsGlobalEvent::RefreshPullRequestMergeability).await?;
                }
                _ = prs_interval.tick() => {
                    refresh_tx.send(BorsGlobalEvent::RefreshPullRequestState).await?;
                }
            }
        }
    };

    let state = ServerState::new(
        repository_tx,
        global_tx,
        WebhookSecret::new(opts.webhook_secret),
        repos,
        db,
        opts.cmd_prefix.clone(),
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
