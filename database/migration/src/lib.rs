pub use sea_orm_migration::prelude::*;

mod m20230505_165859_create_build;
mod m20230506_075859_create_pr;
mod m20230506_102008_create_workflow;
mod m20240312_053916_add_parent_to_build;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20230505_165859_create_build::Migration),
            Box::new(m20230506_075859_create_pr::Migration),
            Box::new(m20230506_102008_create_workflow::Migration),
            Box::new(m20240312_053916_add_parent_to_build::Migration),
        ]
    }
}
