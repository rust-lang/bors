#![allow(async_fn_in_trait)]

//! This is the library of the bors bot.
mod bors;
mod config;
mod database;
mod github;
mod permissions;
mod utils;

pub use bors::{
    event::BorsGlobalEvent, event::BorsRepositoryEvent, BorsContext, BorsContextBuilder,
    CommandParser,
};
pub use database::PgDbClient;
pub use github::{
    api::create_github_client,
    api::create_github_client_from_access_token,
    api::load_repositories,
    server::{create_app, create_bors_process, ServerState},
    WebhookSecret,
};
pub use permissions::TeamApiClient;

#[cfg(test)]
mod tests;
