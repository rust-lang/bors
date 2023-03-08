use sea_orm::DbErr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("Database command error")]
    Database(#[from] DbErr),
    #[error("Entity was not found")]
    NotFound,
}

pub type DatabaseResult<T> = Result<T, DbError>;
