//! Provides access to the database.
use std::{
    fmt::{self, Display, Formatter},
    str::FromStr,
};

use crate::{
    bors::{PullRequestStatus, RollupMode},
    github::{GithubRepoName, PullRequestNumber},
};
use chrono::{DateTime, Utc};
pub use client::PgDbClient;
pub use octocrab::models::pulls::MergeableState as OctocrabMergeableState;
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

        Ok(value.parse()?)
    }
}

impl sqlx::Type<sqlx::Postgres> for PullRequestStatus {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <String as sqlx::Type<sqlx::Postgres>>::type_info()
    }
}

impl sqlx::Encode<'_, sqlx::Postgres> for PullRequestStatus {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, BoxDynError> {
        <String as sqlx::Encode<sqlx::Postgres>>::encode(self.to_string(), buf)
    }
}

impl sqlx::Decode<'_, sqlx::Postgres> for PullRequestStatus {
    fn decode(value: sqlx::postgres::PgValueRef<'_>) -> Result<Self, BoxDynError> {
        let value = <String as sqlx::Decode<sqlx::Postgres>>::decode(value)?;

        Ok(value.parse()?)
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

/// Describes if a pull request can be merged or not.
#[derive(Debug, PartialEq, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "snake_case")]
pub enum MergeableState {
    Mergeable,
    HasConflicts,
    Unknown,
}

impl From<OctocrabMergeableState> for MergeableState {
    fn from(state: OctocrabMergeableState) -> Self {
        match state {
            OctocrabMergeableState::Blocked | OctocrabMergeableState::Dirty => {
                MergeableState::HasConflicts
            }
            OctocrabMergeableState::Clean
            | OctocrabMergeableState::Behind
            | OctocrabMergeableState::HasHooks
            | OctocrabMergeableState::Unstable => MergeableState::Mergeable,
            _ => MergeableState::Unknown,
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "lowercase")]
pub enum DelegatedPermission {
    Try,
    Review,
}

impl fmt::Display for DelegatedPermission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DelegatedPermission::Try => "try",
            DelegatedPermission::Review => "review",
        };
        write!(f, "{}", s)
    }
}

// Has to be kept in sync with the `Display` implementation above.
impl FromStr for DelegatedPermission {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "try" => Ok(DelegatedPermission::Try),
            "review" => Ok(DelegatedPermission::Review),
            _ => Err(format!(
                "Invalid delegation type `{s}`. Possible values are try/review"
            )),
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

impl Display for BuildStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildStatus::Pending => write!(f, "pending"),
            BuildStatus::Success => write!(f, "success"),
            BuildStatus::Failure => write!(f, "failure"),
            BuildStatus::Cancelled => write!(f, "cancelled"),
            BuildStatus::Timeouted => write!(f, "timeouted"),
        }
    }
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
    pub title: String,
    pub pr_status: PullRequestStatus,
    pub base_branch: String,
    pub mergeable_state: MergeableState,
    pub approval_status: ApprovalStatus,
    pub delegated_permission: Option<DelegatedPermission>,
    pub priority: Option<i32>,
    pub rollup: Option<RollupMode>,
    pub try_build: Option<BuildModel>,
    pub created_at: DateTime<Utc>,
}

impl PullRequestModel {
    pub fn is_approved(&self) -> bool {
        matches!(self.approval_status, ApprovalStatus::Approved(_))
    }

    pub fn approver(&self) -> Option<&str> {
        match &self.approval_status {
            ApprovalStatus::Approved(info) => Some(info.approver.as_str()),
            ApprovalStatus::NotApproved => None,
        }
    }

    pub fn approved_sha(&self) -> Option<&str> {
        match &self.approval_status {
            ApprovalStatus::Approved(info) => Some(info.sha.as_str()),
            ApprovalStatus::NotApproved => None,
        }
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

impl TreeState {
    pub fn is_closed(&self) -> bool {
        matches!(self, TreeState::Closed { .. })
    }

    pub fn priority(&self) -> Option<u32> {
        match self {
            TreeState::Closed { priority, .. } => Some(*priority),
            TreeState::Open => None,
        }
    }

    pub fn comment_source(&self) -> Option<&str> {
        match self {
            TreeState::Closed { source, .. } => Some(source),
            TreeState::Open => None,
        }
    }
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
