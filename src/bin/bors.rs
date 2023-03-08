use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::routing::post;
use axum::Router;
use bors::database::orm::SeaORMClient;
use bors::database::DbClient;
use clap::Parser;
use sea_orm::Database;
use tower::limit::ConcurrencyLimitLayer;

use bors::github::api::GithubAppState;
use bors::github::process::github_webhook_process;
use bors::github::server::{github_webhook_handler, ServerState};
use bors::github::{GithubRepoName, WebhookSecret};
use migration::{Migrator, MigratorTrait};

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

    runtime
        .block_on(async move {
            let db = Database::connect("sqlite://bors.db?mode=rwc").await?;
            Migrator::up(&db, None).await?;
            let client = SeaORMClient::new(db);
            let repo = client
                .get_or_create_repo((&GithubRepoName::new("foo", "bar")).into())
                .await?;
            println!("{repo:?}");

            Ok::<_, anyhow::Error>(())
        })
        .unwrap();
    return Ok(());

    let access = runtime.block_on(GithubAppState::load(
        opts.app_id.into(),
        // opts.private_key.into_bytes().into(),
        b"-----BEGIN RSA PRIVATE KEY-----
MIIEpAIBAAKCAQEA6eeDa3NnXu6TNRONZUnZwSvLE6kTcSJgG0R216HLvofhdLGA
XL9D9OieSr9mo56EIkacBB6NqcTxk3kwWdAWWRvaMe/XRyyCy6RrCvRWEFHfdjxQ
fR2+4YyBD8VXV/4se322FTzF+kG7rV2u3NlFmWAe07Ni69E4X+Io9kZ2QbrePN3Z
yK80QhSOskDhGHYfa7n2ycu904ZzoQXyp/5Qx2SYZ+4o3eD7ceTBMlFqM2yHF0Tk
FyZJLPJROfegdiu33tyEJS9foOLDcQz+TcvUwAcw5exaL/9w/qy380fzoUnfJ5K+
acUepKf89MehrLRUUm3idBeOvSmCX6oA1O2OzwIDAQABAoIBAQCGRkclSeyPjLmp
AH5tJQYCZJeBw8/LZIZzYMwwYUtLJ0n/6V3c4FesolUsZ9AOIZOM8afinX+Jc+uS
U0G0bUZHBTwu6pZU33J+YPaqJTW6zKVRhLJYANlxNW1plknb06fJhJMggfDNByss
DNmzIm9X6twHf7VL1qFcOcJ2DmEYvYqY7WT6OlbIj4+5Qu9aDY9fh8d47jufr+Ea
aEiCPWpv0V2AOwgtKC7QSAxVC5Tnc2911uJjnGuep8/OCh3N8oy+sbiEfXdyTImh
IC01pOb94rSttZiI19miwHo0+AanHvZa3eMD4g0/84it5c5yVWTfB4y+6m0Zg/uQ
YzB/NEpRAoGBAPf0IslKpwvp/Tw9oMZvfjA7MtqJtU/JgWOlGGLN2W64Tf5hM8OQ
KSurjHGcioxcWNNmWW/A3Whr3KDYyXWAWgtvedaWS5P3lIxqam5bkb18hqyRgVQe
QYBtfQN97TZt1x4BMu5MEaCwB7/KlE/iGwsNcBiQ+CoUHYvqn0LfEXaLAoGBAPF+
qdmALFLWzX2zFiUa5bc4wN+9WO78SqdeixuWHOdNu2z0sUZecOwXklJ2doTe5l0r
bRfAUioggG9wLZVsZ+0ttHlIyigRhhgfPdNk13Yx8snQldKR0+pDFUv8Ref65EHy
ezVzXOdKJ032WL34BuKngSUQkaQiJXjsun87ypVNAoGBAL95jxdkh8Uih4TqjmpO
lOLIBEhQyWv4zutVBZTfI8Zlmw0SoPenLrPjgMwHN9KWSZ3OTsiG5jOJ/9FSN5h+
aoqkJjE41NpJ+TPJxbC9E7mBHTrMDlQYHTsA0eZNa0552gH4qQzuPzqYVROda5SY
pYuOb/74jDtqVzrCDwSD4CdrAoGALXeunO+/6JzetaLpMXU9+OArmDR7MQu5Nofb
YwdBS99bwWjUk64mTp0lhHcfW2boMnSBpq4kCiBybgjN3Es7yfEIAKnOvfqGp7YC
GvHqiyteTdcCzlF8d6fHs7W8p6+aGDyCLA8bV8SjX89Y5/NxwGzPKN5UvXVcXscb
Wec0/iUCgYB5g+E/OHQxCEcKL1i0JX/jfH6xCnmQolwyim0mrWBRaB5rpV3865bA
qvPEgUn1T80XtYlhjV+xB3YOl6hQdWCW2gpauZdotnY8mM7THMqg23bhWn7jMKUt
0tyH6leFh2C2DdsTICTHYi4oRFDQsNgfbkEKGF3cobe/hYqM4hdVbw==
-----END RSA PRIVATE KEY-----"
            .to_vec()
            .into(),
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
