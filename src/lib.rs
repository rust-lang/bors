#![allow(async_fn_in_trait)]

//! This is the library of the bors bot.
mod bors;
mod config;
mod database;
mod github;
mod permissions;
mod utils;

pub use bors::{BorsContext, CommandParser, event::BorsGlobalEvent, event::BorsRepositoryEvent};
pub use database::PgDbClient;
pub use github::{
    WebhookSecret,
    api::create_github_client,
    api::load_repositories,
    server::{ServerState, create_app, create_bors_process},
};
pub use permissions::TeamApiClient;

#[cfg(test)]
mod tests;
