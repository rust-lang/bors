use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use crate::{PgDbClient, bors::command::CommandParser, github::GithubRepoName};

use super::RepositoryState;

pub struct BorsContext {
    pub parser: CommandParser,
    pub db: Arc<PgDbClient>,
    pub repositories: RwLock<HashMap<GithubRepoName, Arc<RepositoryState>>>,
    web_url: String,
}

impl BorsContext {
    pub fn new(
        parser: CommandParser,
        db: Arc<PgDbClient>,
        repositories: HashMap<GithubRepoName, Arc<RepositoryState>>,
        web_url: &str,
    ) -> Self {
        let repositories = RwLock::new(repositories);
        Self {
            parser,
            db,
            repositories,
            web_url: web_url.trim_end_matches('/').to_string(),
        }
    }

    /// Returns a URL where the bot's website is publicly accessible.
    pub fn get_web_url(&self) -> &str {
        &self.web_url
    }
}
