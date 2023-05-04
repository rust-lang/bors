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
                    .table(TryBuild::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(TryBuild::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(TryBuild::Repository).string().not_null())
                    .col(
                        ColumnDef::new(TryBuild::PullRequestNumber)
                            .integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(TryBuild::CommitSha).string().not_null())
                    .col(
                        ColumnDef::new(TryBuild::CreatedAt)
                            .timestamp()
                            .default(SimpleExpr::Keyword(Keyword::CurrentTimestamp)),
                    )
                    .index(
                        Index::create()
                            .unique()
                            .name("repo-pr-unique")
                            .col(TryBuild::Repository)
                            .col(TryBuild::PullRequestNumber),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(TryBuild::Table).to_owned())
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum TryBuild {
    Table,
    Id,
    Repository,
    PullRequestNumber,
    CommitSha,
    CreatedAt,
}
