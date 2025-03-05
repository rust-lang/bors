//! Provides access to the database.
use std::fmt::{Display, Formatter};

use chrono::{DateTime, Utc};
pub use client::PgDbClient;
use sqlx::error::BoxDynError;

use crate::github::{GithubRepoName, PullRequestNumber};

mod client;
pub(crate) mod operations;

type PrimaryKey = i32;

/// A unique identifier for a workflow run.
#[derive(Clone, Copy, Debug)]
pub struct RunId(pub u64);

/// Postgres doesn't support unsigned integers.
impl sqlx::Type<sqlx::Postgres> for RunId {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <i64 as sqlx::Type<sqlx::Postgres>>::type_info()
    }
}

impl From<i64> for RunId {
    fn from(value: i64) -> RunId {
        RunId(value as u64)
    }
}

impl Display for RunId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl From<RunId> for octocrab::models::RunId {
    fn from(val: RunId) -> Self {
        octocrab::models::RunId(val.0)
    }
}

impl From<octocrab::models::RunId> for RunId {
    fn from(val: octocrab::models::RunId) -> Self {
        RunId(val.0)
    }
}

impl sqlx::Type<sqlx::Postgres> for GithubRepoName {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <String as sqlx::Type<sqlx::Postgres>>::type_info()
    }
}

impl sqlx::Encode<'_, sqlx::Postgres> for GithubRepoName {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, BoxDynError> {
        <String as sqlx::Encode<sqlx::Postgres>>::encode(self.to_string(), buf)
    }
}

impl sqlx::Decode<'_, sqlx::Postgres> for GithubRepoName {
    fn decode(value: sqlx::postgres::PgValueRef<'_>) -> Result<Self, BoxDynError> {
        let value = <String as sqlx::Decode<sqlx::Postgres>>::decode(value)?;
        Ok(Self::from(value))
    }
}

// to load from/ save to Postgres, as it doesn't have unsigned integer types.
impl From<i64> for PullRequestNumber {
    fn from(value: i64) -> Self {
        Self(value as u64)
    }
}

impl sqlx::Type<sqlx::Postgres> for PullRequestNumber {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        // Postgres don't have unsigned integer types.
        <i64 as sqlx::Type<sqlx::Postgres>>::type_info()
    }
}

impl sqlx::Encode<'_, sqlx::Postgres> for TreeState {
    fn encode_by_ref(&self, buf: &mut sqlx::postgres::PgArgumentBuffer) -> Result<sqlx::encode::IsNull, Box<dyn std::error::Error + Send + Sync>> {
        let value = match self {
            TreeState::Open => String::from("open"),
            TreeState::Closed(priority) => format!("closed:{}", priority),
        };
        buf.extend(value.as_bytes());
        Ok(sqlx::encode::IsNull::No)
    }
}

impl sqlx::Decode<'_, sqlx::Postgres> for TreeState {
    fn decode(value: sqlx::postgres::PgValueRef<'_>) -> Result<Self, sqlx::error::BoxDynError> {
        let value = <&str as sqlx::Decode<sqlx::Postgres>>::decode(value)?;
        match value {
            "open" => Ok(TreeState::Open),
            value if value.starts_with("closed:") => {
                let priority = value[7..].parse()?;
                Ok(TreeState::Closed(priority))
            }
            _ => Err("invalid tree state".into()),
        }
    }
}


/// Status of a GitHub build.
#[derive(Debug, Clone, PartialEq, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "lowercase")]
pub enum BuildStatus {
    /// The build is still waiting for results.
    Pending,
    /// The build has succeeded.
    Success,
    /// The build has failed.
    Failure,
    /// The build has been manually cancelled by a user.
    Cancelled,
    /// The build ran for too long and was timeouted by the bot.
    Timeouted,
}

/// Represents a single (merged) commit.
#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "build")]
pub struct BuildModel {
    pub id: PrimaryKey,
    pub repository: GithubRepoName,
    pub branch: String,
    pub commit_sha: String,
    pub status: BuildStatus,
    pub parent: String,
    pub created_at: DateTime<Utc>,
}

/// Represents a pull request.
#[derive(Debug, Clone)]
pub struct PullRequestModel {
    pub id: PrimaryKey,
    pub repository: GithubRepoName,
    pub number: PullRequestNumber,
    pub approved_by: Option<String>,
    pub priority: Option<i32>,
    pub try_build: Option<BuildModel>,
    pub created_at: DateTime<Utc>,
}

impl PullRequestModel {
    pub fn is_approved(&self) -> bool {
        self.approved_by.is_some()
    }
}

/// Describes whether a workflow is a Github Actions workflow or if it's a job from some external
/// CI.
#[derive(Debug, PartialEq, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "lowercase")]
pub enum WorkflowType {
    Github,
    External,
}

/// Status of a workflow.
#[derive(Debug, PartialEq, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "lowercase")]
pub enum WorkflowStatus {
    /// Workflow is running.
    Pending,
    /// Workflow has succeeded.
    Success,
    /// Workflow has failed.
    Failure,
}

/// Represents a workflow run, coming either from Github Actions or from some external CI.
pub struct WorkflowModel {
    pub id: PrimaryKey,
    pub build: BuildModel,
    pub name: String,
    pub url: String,
    pub run_id: RunId,
    pub workflow_type: WorkflowType,
    pub status: WorkflowStatus,
    pub created_at: DateTime<Utc>,
}

/// Represents the state of a repository's tree.
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum TreeState {
    /// The repository tree is open for changes
    Open,
    /// The repository tree is closed to changes with a priority threshold
    Closed(u32),
}

impl sqlx::Type<sqlx::Postgres> for TreeState {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <String as sqlx::Type<sqlx::Postgres>>::type_info()
    }
}

/// Represents a repository configuration.
pub struct RepoModel {
    pub id: PrimaryKey,
    pub repo: GithubRepoName,
    pub treeclosed: TreeState,
    pub treeclosed_src: Option<String>,
    pub created_at: DateTime<Utc>,
}
