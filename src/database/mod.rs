//! Provides access to the database.
use std::{
    fmt::{Display, Formatter},
    str::FromStr,
};

use crate::{
    bors::RollupMode,
    github::{GithubRepoName, PullRequestNumber},
};
use chrono::{DateTime, Utc};
pub use client::PgDbClient;
use sqlx::error::BoxDynError;
use sqlx::{Database, Postgres};

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
        let value = <&str as sqlx::Decode<sqlx::Postgres>>::decode(value)?;
        Ok(value.parse()?)
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
        // Postgres doesn't have unsigned integer types.
        <i64 as sqlx::Type<sqlx::Postgres>>::type_info()
    }
}

impl sqlx::Type<sqlx::Postgres> for RollupMode {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <String as sqlx::Type<sqlx::Postgres>>::type_info() // Store as TEXT in Postgres
    }
}

impl sqlx::Encode<'_, sqlx::Postgres> for RollupMode {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, BoxDynError> {
        <String as sqlx::Encode<sqlx::Postgres>>::encode(self.to_string(), buf)
    }
}

impl sqlx::Decode<'_, sqlx::Postgres> for RollupMode {
    fn decode(value: sqlx::postgres::PgValueRef<'_>) -> Result<Self, BoxDynError> {
        let value = <String as sqlx::Decode<sqlx::Postgres>>::decode(value)?;

        Self::from_str(&value).map_err(|_| panic!("Invalid RollupMode value: {}", value))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalInfo {
    /// The user who approved the pull request.
    pub approver: String,
    /// The SHA of the commit that was approved.
    pub sha: String,
}

/// Represents the approval status of a pull request.
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalStatus {
    NotApproved,
    Approved(ApprovalInfo),
}

impl ApprovalStatus {
    pub fn approver(&self) -> Option<&str> {
        match self {
            ApprovalStatus::Approved(info) => Some(info.approver.as_str()),
            ApprovalStatus::NotApproved => None,
        }
    }

    pub fn sha(&self) -> Option<&str> {
        match self {
            ApprovalStatus::Approved(info) => Some(info.sha.as_str()),
            ApprovalStatus::NotApproved => None,
        }
    }
}

impl sqlx::Type<sqlx::Postgres> for ApprovalStatus {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <(Option<String>, Option<String>) as sqlx::Type<sqlx::Postgres>>::type_info()
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Postgres> for ApprovalStatus {
    fn decode(value: sqlx::postgres::PgValueRef<'r>) -> Result<Self, BoxDynError> {
        let (approver, sha) =
            <(Option<String>, Option<String>) as sqlx::Decode<sqlx::Postgres>>::decode(value)?;

        match (approver, sha) {
            (Some(approver), Some(sha)) => {
                Ok(ApprovalStatus::Approved(ApprovalInfo { approver, sha }))
            }
            (None, None) => Ok(ApprovalStatus::NotApproved),
            (approver, sha) => Err(format!(
                "Inconsistent approval state: approver={:?}, sha={:?}",
                approver, sha
            )
            .into()),
        }
    }
}

/// Status of a GitHub build.
#[derive(Debug, PartialEq, sqlx::Type)]
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
#[derive(Debug, sqlx::Type)]
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
#[derive(Debug)]
pub struct PullRequestModel {
    pub id: PrimaryKey,
    pub repository: GithubRepoName,
    pub number: PullRequestNumber,
    pub approval_status: ApprovalStatus,
    pub delegated: bool,
    pub priority: Option<i32>,
    pub rollup: Option<RollupMode>,
    pub try_build: Option<BuildModel>,
    pub created_at: DateTime<Utc>,
}

impl PullRequestModel {
    pub fn is_approved(&self) -> bool {
        matches!(self.approval_status, ApprovalStatus::Approved(_))
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
#[derive(Debug, PartialEq, Clone)]
pub enum TreeState {
    /// The repository tree is open for changes
    Open,
    /// The repository tree is closed to changes with a priority threshold
    Closed {
        /// PRs with priority lower than this value cannot be merged.
        priority: u32,
        /// URL to a PR comment that closed the tree.
        source: String,
    },
}

impl sqlx::Type<sqlx::Postgres> for TreeState {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <(Option<i32>, Option<String>) as sqlx::Type<sqlx::Postgres>>::type_info()
    }
}

impl sqlx::Decode<'_, sqlx::Postgres> for TreeState {
    fn decode(value: <Postgres as Database>::ValueRef<'_>) -> Result<Self, BoxDynError> {
        let data = <(Option<i32>, Option<String>) as sqlx::Decode<sqlx::Postgres>>::decode(value)?;
        match data {
            (Some(priority), Some(source)) => Ok(TreeState::Closed {
                priority: priority as u32,
                source,
            }),
            (None, None) => Ok(TreeState::Open),
            _ => Err(
                "Cannot deserialize TreeState, priority is non-NULL, but source is NULL"
                    .to_string()
                    .into(),
            ),
        }
    }
}

/// Represents a repository configuration.
pub struct RepoModel {
    pub id: PrimaryKey,
    pub name: GithubRepoName,
    pub tree_state: TreeState,
    pub created_at: DateTime<Utc>,
}
