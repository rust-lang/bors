use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Build::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Build::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Build::Repository).string().not_null())
                    .col(ColumnDef::new(Build::Branch).string().not_null())
                    .col(ColumnDef::new(Build::CommitSha).string().not_null())
                    .col(ColumnDef::new(Build::Status).string().not_null())
                    .col(
                        ColumnDef::new(Build::CreatedAt)
                            .timestamp()
                            .default(SimpleExpr::Keyword(Keyword::CurrentTimestamp))
                            .not_null(),
                    )
                    .index(
                        Index::create()
                            .unique()
                            .name("unique-build-repo-branch-commit")
                            .col(Build::Repository)
                            .col(Build::Branch)
                            .col(Build::CommitSha),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Build::Table).to_owned())
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
pub enum Build {
    Table,
    Id,
    Repository,
    Branch,
    CommitSha,
    Status,
    CreatedAt,
}
