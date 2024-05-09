//! This is the library of the bors bot.
mod bors;
mod config;
mod database;
mod github;
mod permissions;
mod utils;

pub use bors::{event::BorsGlobalEvent, event::BorsRepositoryEvent, BorsContext, CommandParser};
pub use database::SeaORMClient;
pub use github::{
    api::create_github_client,
    server::{create_app, create_bors_process, ServerState},
    WebhookSecret,
};
pub use permissions::TeamApiClient;

#[cfg(test)]
mod tests;
