use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use crate::{bors::command::CommandParser, github::GithubRepoName, PgDbClient};

use super::{RepositoryClient, RepositoryState};

pub struct BorsContext<Client: RepositoryClient> {
    pub parser: CommandParser,
    pub db: Arc<PgDbClient>,
    pub repositories: RwLock<HashMap<GithubRepoName, Arc<RepositoryState<Client>>>>,
}

impl<Client: RepositoryClient> BorsContext<Client> {
    pub fn new(
        parser: CommandParser,
        db: Arc<PgDbClient>,
        repositories: HashMap<GithubRepoName, Arc<RepositoryState<Client>>>,
    ) -> Self {
        let repositories = RwLock::new(repositories);
        Self {
            parser,
            db,
            repositories,
        }
    }
}
