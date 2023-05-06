use anyhow::anyhow;
use axum::async_trait;
use sea_orm::sea_query::OnConflict;
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, IntoActiveModel, QueryFilter};

use entity::build::Model;
use entity::{build, pull_request};
use migration::sea_orm::DatabaseConnection;

use crate::database::DbClient;
use crate::github::PullRequestNumber;
use crate::github::{CommitSha, GithubRepoName};

pub struct SeaORMClient {
    db: DatabaseConnection,
}

impl SeaORMClient {
    pub fn new(connection: DatabaseConnection) -> Self {
        Self { db: connection }
    }
}

#[async_trait]
impl DbClient for SeaORMClient {
    async fn get_or_create_pull_request(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
    ) -> anyhow::Result<pull_request::Model> {
        let pr = pull_request::ActiveModel {
            repository: Set(full_repo_name(repo)),
            number: Set(pr_number.0 as i32),
            ..Default::default()
        };

        // Try to insert first
        match pull_request::Entity::insert(pr)
            .on_conflict(OnConflict::new().do_nothing().to_owned())
            .exec_with_returning(&self.db)
            .await
        {
            Ok(_) => {}
            Err(DbErr::RecordNotInserted) => {
                // The record was already in DB
            }
            Err(error) => return Err(error.into()),
        };

        // Resolve the PR row
        let pr = pull_request::Entity::find()
            .filter(
                pull_request::Column::Repository
                    .eq(full_repo_name(repo))
                    .and(pull_request::Column::Number.eq(pr_number.0)),
            )
            .one(&self.db)
            .await?
            .ok_or_else(|| anyhow!("Cannot find PR row"))?;

        Ok(pr)
    }

    async fn attach_try_build(
        &self,
        pr: pull_request::Model,
        commit_sha: CommitSha,
    ) -> anyhow::Result<()> {
        let build = build::ActiveModel {
            commit_sha: Set(commit_sha.0),
            repository: Set(pr.repository.clone()),
            ..Default::default()
        };
        let build = build::Entity::insert(build)
            .exec_with_returning(&self.db)
            .await
            .map_err(|error| anyhow!("Cannot insert build into DB: {error:?}"))?;

        let mut pr_model = pr.into_active_model();
        pr_model.try_build = Set(Some(build.id));
        pr_model.update(&self.db).await?;

        Ok(())
    }

    async fn find_build_by_commit(
        &self,
        repo: &GithubRepoName,
        commit_sha: CommitSha,
    ) -> anyhow::Result<Option<Model>> {
        Ok(build::Entity::find()
            .filter(
                build::Column::Repository
                    .eq(full_repo_name(repo))
                    .and(build::Column::CommitSha.eq(commit_sha.0)),
            )
            .one(&self.db)
            .await?)
    }

    // async fn find_try_build_by_commit(
    //     &self,
    //     repo: &GithubRepoName,
    //     commit: &CommitSha,
    // ) -> anyhow::Result<Option<try_build::Model>> {
    //     let mut entries = try_build::Entity::find()
    //         .filter(
    //             try_build::Column::Repository
    //                 .eq(full_repo_name(repo))
    //                 .and(try_build::Column::CommitSha.eq(&commit.0)),
    //         )
    //         .all(&self.db)
    //         .await?;
    //     Ok(entries.pop())
    // }
    //
    // async fn find_try_build_for_pr(
    //     &self,
    //     repo: &GithubRepoName,
    //     pr_number: PullRequestNumber,
    // ) -> anyhow::Result<Option<try_build::Model>> {
    //     let mut entries = try_build::Entity::find()
    //         .filter(
    //             try_build::Column::Repository
    //                 .eq(full_repo_name(repo))
    //                 .and(try_build::Column::PullRequestNumber.eq(pr_number.0)),
    //         )
    //         .all(&self.db)
    //         .await?;
    //     Ok(entries.pop())
    // }
    //
    // async fn delete_try_build(&self, model: try_build::Model) -> anyhow::Result<()> {
    //     try_build::Entity::delete_by_id(model.id)
    //         .exec(&self.db)
    //         .await?;
    //     Ok(())
    // }
}

fn full_repo_name(repo: &GithubRepoName) -> String {
    format!("{}/{}", repo.owner(), repo.name())
}
