#![allow(async_fn_in_trait)]

//! This is the library of the bors bot.
mod bors;
mod config;
mod database;
mod github;
mod permissions;
pub mod server;
mod templates;
mod utils;

pub use self::bors::process::{BorsProcess, create_bors_process};
pub use bors::{
    BorsContext, CommandParser, Git, RepositoryStore, event::BorsGlobalEvent,
    event::BorsRepositoryEvent,
};
pub use database::{PgDbClient, TreeState};
pub use github::{
    AppError, OAuthClient, OAuthConfig, WebhookSecret, api::create_github_client,
    api::load_repositories,
};
pub use permissions::TeamApiClient;
pub use server::ServerState;
pub use server::create_app;

/// This migrator serves for including a single place in the test code that stores all migrations
/// loaded from disk. All sqlx tests should then reference it using
/// `#[sqlx::test(migrator = "crate::MIGRATOR"]`.
///
/// This makes tests much faster to rebuild, otherwise each test would load the migrations and
/// crucially include their copy in the source code for each test, which bloats the binary and
/// makes compilation slower.
#[cfg(test)]
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

#[cfg(test)]
mod tests;
