use crate::m20230505_165859_create_build::Build;
use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_query::types::Keyword;
use sea_orm_migration::sea_query::SimpleExpr;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(PullRequest::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(PullRequest::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(PullRequest::Repository).string().not_null())
                    .col(ColumnDef::new(PullRequest::Number).integer().not_null())
                    .col(ColumnDef::new(PullRequest::TryBuild).integer().null())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk-pr-try-build")
                            .from(PullRequest::Table, PullRequest::TryBuild)
                            .to(Build::Table, Build::Id),
                    )
                    .col(
                        ColumnDef::new(PullRequest::CreatedAt)
                            .timestamp()
                            .default(SimpleExpr::Keyword(Keyword::CurrentTimestamp))
                            .not_null(),
                    )
                    .index(
                        Index::create()
                            .unique()
                            .name("unique-repo-number")
                            .col(PullRequest::Repository)
                            .col(PullRequest::Number),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(PullRequest::Table).to_owned())
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum PullRequest {
    Table,
    Id,
    Repository,
    Number,
    TryBuild,
    CreatedAt,
}
