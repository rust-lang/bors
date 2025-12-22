use std::sync::Arc;

use crate::{PgDbClient, bors::command::CommandParser, github::GithubRepoName};

use super::{RepositoryState, RepositoryStore};

pub struct BorsContext {
    pub parser: CommandParser,
    pub db: Arc<PgDbClient>,
    pub repositories: Arc<RepositoryStore>,
    web_url: String,
}

impl BorsContext {
    pub fn new(
        parser: CommandParser,
        db: Arc<PgDbClient>,
        repositories: Arc<RepositoryStore>,
        web_url: &str,
    ) -> Self {
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

    pub fn get_repo(&self, name: &GithubRepoName) -> anyhow::Result<Arc<RepositoryState>> {
        let repo_state = match self.repositories.get(name) {
            Some(state) => state.clone(),
            None => {
                return Err(anyhow::anyhow!("Repository not found: {name}"));
            }
        };
        Ok(repo_state)
    }
}
