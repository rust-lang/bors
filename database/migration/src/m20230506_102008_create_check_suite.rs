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
                    .table(CheckSuite::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(CheckSuite::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(CheckSuite::Build).integer().not_null())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk-workflow-build")
                            .from(CheckSuite::Table, CheckSuite::Build)
                            .to(Build::Table, Build::Id),
                    )
                    .col(
                        ColumnDef::new(CheckSuite::CheckSuiteId)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(CheckSuite::WorkflowRunId)
                            .big_unsigned()
                            .null(),
                    )
                    .col(ColumnDef::new(CheckSuite::Status).string().not_null())
                    .col(
                        ColumnDef::new(CheckSuite::CreatedAt)
                            .timestamp()
                            .default(SimpleExpr::Keyword(Keyword::CurrentTimestamp))
                            .not_null(),
                    )
                    .index(
                        Index::create()
                            .unique()
                            .name("unique-check-suite-build-check-suite-id")
                            .col(CheckSuite::Build)
                            .col(CheckSuite::CheckSuiteId),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(CheckSuite::Table).to_owned())
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum CheckSuite {
    Table,
    Id,
    Build,
    CheckSuiteId,
    WorkflowRunId,
    Status,
    CreatedAt,
}
