use sea_orm::Database;

use migration::{Migrator, MigratorTrait};

use crate::database::SeaORMClient;

pub async fn create_test_db() -> SeaORMClient {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    SeaORMClient::new(db.clone())
}
