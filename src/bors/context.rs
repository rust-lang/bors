use crate::{bors::command::CommandParser, database::DbClient};
use std::sync::Arc;

pub struct BorsContext {
    pub parser: CommandParser,
    pub db: Arc<dyn DbClient>,
}

impl BorsContext {
    pub fn new(parser: CommandParser, db: Arc<dyn DbClient>) -> Self {
        Self { parser, db }
    }
}
