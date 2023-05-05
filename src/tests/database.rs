use migration::{Migrator, MigratorTrait};
use sea_orm::Database;

use crate::database::SeaORMClient;

pub async fn create_inmemory_db() -> SeaORMClient {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    SeaORMClient::new(db)
}
