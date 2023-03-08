use axum::async_trait;
use sea_orm::ActiveValue::Set;
use sea_orm::{DbErr, EntityTrait};

use entity::repository::Entity;
use entity::Repository;
use migration::OnConflict;

use crate::database::error::DbError;
use crate::database::{DatabaseResult, DbClient, RepositoryId};

pub struct SeaORMClient {
    conn: sea_orm::DbConn,
}

impl SeaORMClient {
    pub fn new(conn: sea_orm::DbConn) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl DbClient for SeaORMClient {
    async fn get_or_create_repo(&self, repo: RepositoryId) -> DatabaseResult<Repository> {
        use entity::repository::{ActiveModel, Column};

        let on_conflict = OnConflict::column(Column::Id).do_nothing().to_owned();

        match Entity::insert(ActiveModel {
            id: Set(repo.0.clone()),
            ..Default::default()
        })
        .on_conflict(on_conflict)
        .exec(&self.conn)
        .await
        {
            Ok(_) | Err(DbErr::RecordNotInserted) => {}
            Err(error) => return Err(error.into()),
        }

        let repo = Entity::find_by_id(repo.0)
            .one(&self.conn)
            .await?
            .ok_or_else(|| DbError::NotFound)?;
        Ok(repo)
    }
}
