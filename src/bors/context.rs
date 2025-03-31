use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use crate::{PgDbClient, bors::command::CommandParser, github::GithubRepoName};

use super::{RepositoryState, mergeable_queue::MergeableQueue};

pub struct BorsContext {
    pub parser: CommandParser,
    pub db: Arc<PgDbClient>,
    pub repositories: RwLock<HashMap<GithubRepoName, Arc<RepositoryState>>>,
    pub mergeable_queue: Arc<MergeableQueue>,
}

impl BorsContext {
    pub fn new(
        parser: CommandParser,
        db: Arc<PgDbClient>,
        repositories: HashMap<GithubRepoName, Arc<RepositoryState>>,
    ) -> Self {
        let repositories = RwLock::new(repositories);
        let mergeable_queue = Arc::new(MergeableQueue::new());
        Self {
            parser,
            db,
            repositories,
            mergeable_queue,
        }
    }
}
