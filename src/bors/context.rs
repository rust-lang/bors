use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use crate::{bors::command::CommandParser, database::DbClient, github::GithubRepoName};

use super::{RepositoryClient, RepositoryState};

pub struct BorsContext<Client: RepositoryClient> {
    pub parser: CommandParser,
    pub db: Arc<dyn DbClient>,
    pub repositories: RwLock<HashMap<GithubRepoName, Arc<RepositoryState<Client>>>>,
}

impl<Client: RepositoryClient> BorsContext<Client> {
    pub fn new(
        parser: CommandParser,
        db: Arc<dyn DbClient>,
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
