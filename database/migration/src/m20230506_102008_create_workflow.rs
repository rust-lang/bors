use sea_orm_migration::prelude::*;

use crate::m20230505_165859_create_build::Build;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Workflow::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Workflow::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Workflow::Build).integer().not_null())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk-workflow-build")
                            .from(Workflow::Table, Workflow::Build)
                            .to(Build::Table, Build::Id),
                    )
                    .col(ColumnDef::new(Workflow::Name).string().not_null())
                    .col(ColumnDef::new(Workflow::RunId).big_unsigned().null())
                    .col(ColumnDef::new(Workflow::Url).string().not_null())
                    .col(ColumnDef::new(Workflow::Status).string().not_null())
                    .col(
                        ColumnDef::new(Workflow::CreatedAt)
                            .timestamp()
                            .default(SimpleExpr::Keyword(Keyword::CurrentTimestamp))
                            .not_null(),
                    )
                    .index(
                        Index::create()
                            .unique()
                            .name("unique-workflow-build-url")
                            .col(Workflow::Build)
                            .col(Workflow::Url),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Workflow::Table).to_owned())
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum Workflow {
    Table,
    Id,
    Build,
    Name,
    RunId,
    Url,
    Status,
    CreatedAt,
}
