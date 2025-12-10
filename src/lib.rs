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

pub use bors::{BorsContext, CommandParser, event::BorsGlobalEvent, event::BorsRepositoryEvent};
pub use database::{PgDbClient, TreeState};
pub use github::{
    AppError, WebhookSecret,
    api::create_github_client,
    api::load_repositories,
    process::{BorsProcess, create_bors_process},
};
pub use permissions::TeamApiClient;
pub use server::OAuthConfig;
pub use server::ServerState;
pub use server::create_app;

#[cfg(test)]
mod tests;
