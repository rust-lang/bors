use axum::async_trait;

use crate::database::error::DatabaseResult;
use entity::Repository;

pub mod error;
pub mod orm;

pub struct RepositoryId(pub String);

#[async_trait]
pub trait DbClient {
    async fn get_or_create_repo(&self, repo: RepositoryId) -> DatabaseResult<Repository>;
}
