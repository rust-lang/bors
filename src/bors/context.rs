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
}

impl BorsContext {
    pub fn new(
        parser: CommandParser,
        db: Arc<PgDbClient>,
        repositories: HashMap<GithubRepoName, Arc<RepositoryState>>,
    ) -> Self {
        let repositories = RwLock::new(repositories);
        Self {
            parser,
            db,
            repositories,
        }
    }
}
