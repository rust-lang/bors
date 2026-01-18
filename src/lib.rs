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

#[cfg(test)]
mod tests;
