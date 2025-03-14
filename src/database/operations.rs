use chrono::DateTime;
use chrono::Utc;
use sqlx::postgres::PgExecutor;

use crate::bors::RollupMode;
use crate::database::BuildStatus;
use crate::database::RepoModel;
use crate::database::WorkflowModel;
use crate::github::CommitSha;
use crate::github::GithubRepoName;
use crate::github::PullRequestNumber;

use super::ApprovalInfo;
use super::ApprovalStatus;
use super::BuildModel;
use super::PullRequestModel;
use super::RunId;
use super::TreeState;
use super::WorkflowStatus;
use super::WorkflowType;

pub(crate) async fn get_pull_request(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    pr_number: PullRequestNumber,
) -> anyhow::Result<Option<PullRequestModel>> {
    let record = sqlx::query_as!(
        PullRequestModel,
        r#"
    SELECT
        pr.id,
        pr.repository as "repository: GithubRepoName",
        pr.number as "number!: i64",
        (
            pr.approved_by,
            pr.approved_sha
        ) AS "approval_status!: ApprovalStatus",
        pr.priority,
        pr.rollup as "rollup: RollupMode",
        pr.delegated,
        pr.created_at as "created_at: DateTime<Utc>",
        build AS "try_build: BuildModel"
    FROM pull_request as pr
    LEFT JOIN build ON pr.build_id = build.id
    WHERE pr.repository = $1 AND
          pr.number = $2
    "#,
        repo as &GithubRepoName,
        pr_number.0 as i32
    )
    .fetch_optional(executor)
    .await?;

    Ok(record)
}

pub(crate) async fn create_pull_request(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    pr_number: PullRequestNumber,
) -> anyhow::Result<()> {
    sqlx::query!(
        r#"
INSERT INTO pull_request (repository, number)
VALUES ($1, $2) ON CONFLICT DO NOTHING
"#,
        repo as &GithubRepoName,
        pr_number.0 as i32
    )
    .execute(executor)
    .await?;
    Ok(())
}

pub(crate) async fn approve_pull_request(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
    approval_info: ApprovalInfo,
    priority: Option<u32>,
    rollup: Option<RollupMode>,
) -> anyhow::Result<()> {
    let priority_i32 = priority.map(|p| p as i32);

    sqlx::query!(
        r#"
UPDATE pull_request
SET approved_by = $1,
    approved_sha = $2,
    priority = COALESCE($3, priority),
    rollup = COALESCE($4, rollup)
WHERE id = $5
"#,
        approval_info.approver,
        approval_info.sha,
        priority_i32,
        rollup.map(|r| r.to_string()),
        pr_id,
    )
    .execute(executor)
    .await?;
    Ok(())
}

pub(crate) async fn unapprove_pull_request(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
) -> anyhow::Result<()> {
    sqlx::query!(
        "UPDATE pull_request SET approved_by = NULL, approved_sha = NULL WHERE id = $1",
        pr_id
    )
    .execute(executor)
    .await?;
    Ok(())
}

pub(crate) async fn delegate_pull_request(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
) -> anyhow::Result<()> {
    sqlx::query!(
        "UPDATE pull_request SET delegated = TRUE WHERE id = $1",
        pr_id,
    )
    .execute(executor)
    .await?;
    Ok(())
}

pub(crate) async fn undelegate_pull_request(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
) -> anyhow::Result<()> {
    sqlx::query!(
        "UPDATE pull_request SET delegated = FALSE WHERE id = $1",
        pr_id
    )
    .execute(executor)
    .await?;
    Ok(())
}

pub(crate) async fn find_pr_by_build(
    executor: impl PgExecutor<'_>,
    build_id: i32,
) -> anyhow::Result<Option<PullRequestModel>> {
    let record = sqlx::query_as!(
        PullRequestModel,
        r#"
SELECT
    pr.id,
    pr.repository as "repository: GithubRepoName",
    pr.number as "number!: i64",
    (
        pr.approved_by,
        pr.approved_sha
    ) AS "approval_status!: ApprovalStatus",
    pr.delegated,
    pr.priority,
    pr.rollup as "rollup: RollupMode",
    pr.created_at as "created_at: DateTime<Utc>",
    build AS "try_build: BuildModel"
FROM pull_request as pr
LEFT JOIN build ON pr.build_id = build.id
WHERE build.id = $1
"#,
        build_id
    )
    .fetch_optional(executor)
    .await?;

    Ok(record)
}

pub(crate) async fn update_pr_build_id(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
    build_id: i32,
) -> anyhow::Result<()> {
    sqlx::query!(
        "UPDATE pull_request SET build_id = $1 WHERE id = $2",
        build_id,
        pr_id
    )
    .execute(executor)
    .await?;
    Ok(())
}

pub(crate) async fn create_build(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    branch: &str,
    commit_sha: &CommitSha,
    parent: &CommitSha,
) -> anyhow::Result<i32> {
    let build_id = sqlx::query_scalar!(
        r#"
INSERT INTO build (repository, branch, commit_sha, parent, status)
VALUES ($1, $2, $3, $4, $5)
RETURNING id
"#,
        repo as &GithubRepoName,
        branch,
        commit_sha.0,
        parent.0,
        BuildStatus::Pending as BuildStatus
    )
    .fetch_one(executor)
    .await?;
    Ok(build_id)
}

pub(crate) async fn find_build(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    branch: &str,
    commit_sha: &CommitSha,
) -> anyhow::Result<Option<BuildModel>> {
    let build = sqlx::query_as!(
        BuildModel,
        r#"
SELECT
    id,
    repository as "repository: GithubRepoName",
    branch,
    commit_sha,
    parent,
    status as "status: BuildStatus",
    created_at as "created_at: DateTime<Utc>"
FROM build
WHERE repository = $1
    AND branch = $2
    AND commit_sha = $3
"#,
        repo as &GithubRepoName,
        branch,
        commit_sha.0
    )
    .fetch_optional(executor)
    .await?;
    Ok(build)
}

pub(crate) async fn get_running_builds(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
) -> anyhow::Result<Vec<BuildModel>> {
    let builds = sqlx::query_as!(
        BuildModel,
        r#"
SELECT
    id,
    repository as "repository: GithubRepoName",
    branch,
    commit_sha,
    parent,
    status as "status: BuildStatus",
    created_at as "created_at: DateTime<Utc>"
FROM build
WHERE repository = $1
    AND status = $2
"#,
        repo as &GithubRepoName,
        BuildStatus::Pending as BuildStatus
    )
    .fetch_all(executor)
    .await?;
    Ok(builds)
}

pub(crate) async fn update_build_status(
    executor: impl PgExecutor<'_>,
    build_id: i32,
    status: BuildStatus,
) -> anyhow::Result<()> {
    sqlx::query!(
        "UPDATE build SET status = $1 WHERE id = $2",
        status as _,
        build_id
    )
    .execute(executor)
    .await?;
    Ok(())
}

pub(crate) async fn create_workflow(
    executor: impl PgExecutor<'_>,
    build_id: i32,
    name: &str,
    url: &str,
    run_id: RunId,
    workflow_type: WorkflowType,
    status: WorkflowStatus,
) -> anyhow::Result<()> {
    sqlx::query!(
        r#"
INSERT INTO workflow (build_id, name, url, run_id, type, status)
VALUES ($1, $2, $3, $4, $5, $6)
"#,
        build_id,
        name,
        url,
        run_id.0 as i64,
        workflow_type as _,
        status as _
    )
    .execute(executor)
    .await?;
    Ok(())
}

pub(crate) async fn update_workflow_status(
    executor: impl PgExecutor<'_>,
    run_id: u64,
    status: WorkflowStatus,
) -> anyhow::Result<()> {
    sqlx::query!(
        "UPDATE workflow SET status = $1 WHERE run_id = $2",
        status as _,
        run_id as i64
    )
    .execute(executor)
    .await?;
    Ok(())
}

pub(crate) async fn set_pr_priority(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
    priority: u32,
) -> anyhow::Result<()> {
    sqlx::query!(
        "UPDATE pull_request SET priority = $1 WHERE id = $2",
        priority as i32,
        pr_id,
    )
    .execute(executor)
    .await?;
    Ok(())
}

pub(crate) async fn set_pr_rollup(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
    rollup: RollupMode,
) -> anyhow::Result<()> {
    sqlx::query!(
        "UPDATE pull_request SET rollup = $1 WHERE id = $2",
        rollup.to_string(),
        pr_id,
    )
    .execute(executor)
    .await?;
    Ok(())
}

pub(crate) async fn get_workflows_for_build(
    executor: impl PgExecutor<'_>,
    build_id: i32,
) -> anyhow::Result<Vec<WorkflowModel>> {
    let workflows = sqlx::query_as!(
        WorkflowModel,
        r#"
SELECT
    workflow.id,
    workflow.name,
    workflow.url,
    workflow.run_id,
    workflow.type as "workflow_type: WorkflowType",
    workflow.status as "status: WorkflowStatus",
    workflow.created_at as "created_at: DateTime<Utc>",
    (
        build.id,
        build.repository,
        build.branch,
        build.commit_sha,
        build.status,
        build.parent,
        build.created_at
    ) AS "build!: BuildModel"
FROM workflow
    LEFT JOIN build ON workflow.build_id = build.id
WHERE build.id = $1
"#,
        build_id
    )
    .fetch_all(executor)
    .await?;
    Ok(workflows)
}

pub(crate) async fn get_workflow_urls_for_build(
    executor: impl PgExecutor<'_>,
    build_id: i32,
) -> anyhow::Result<Vec<String>> {
    let results = sqlx::query!(
        r#"
SELECT url
FROM workflow
WHERE build_id = $1
"#,
        build_id
    )
    .fetch_all(executor)
    .await?;

    Ok(results.into_iter().map(|r| r.url).collect())
}

#[cfg(test)]
pub(crate) async fn get_all_workflows(
    executor: impl PgExecutor<'_>,
) -> anyhow::Result<Vec<WorkflowModel>> {
    let workflows = sqlx::query_as!(
        WorkflowModel,
        r#"
SELECT
    workflow.id,
    workflow.name,
    workflow.url,
    workflow.run_id,
    workflow.type as "workflow_type: WorkflowType",
    workflow.status as "status: WorkflowStatus",
    workflow.created_at as "created_at: DateTime<Utc>",
    (
        build.id,
        build.repository,
        build.branch,
        build.commit_sha,
        build.status,
        build.parent,
        build.created_at
    ) AS "build!: BuildModel"
FROM workflow
    LEFT JOIN build ON workflow.build_id = build.id
"#
    )
    .fetch_all(executor)
    .await?;
    Ok(workflows)
}

/// Retrieves a repository from the database or creates it if it does not exist.
pub(crate) async fn get_repository(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
) -> anyhow::Result<Option<RepoModel>> {
    let repo = sqlx::query_as!(
        RepoModel,
        r#"
        SELECT
            id,
            name as "name: GithubRepoName",
            (
                tree_state,
                treeclosed_src
            ) AS "tree_state!: TreeState",
            created_at
        FROM repository
        WHERE name = $1
        "#,
        repo as &GithubRepoName
    )
    .fetch_optional(executor)
    .await?;

    Ok(repo)
}

/// Updates the tree state of a repository.
pub(crate) async fn upsert_repository(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    tree_state: TreeState,
) -> anyhow::Result<()> {
    let (priority, src) = match tree_state {
        TreeState::Open => (None, None),
        TreeState::Closed { priority, source } => (Some(priority as i32), Some(source)),
    };
    sqlx::query!(
        r#"
        INSERT INTO repository (name, tree_state, treeclosed_src)
        VALUES ($1, $2, $3)
        ON CONFLICT (name)
        DO UPDATE SET tree_state = EXCLUDED.tree_state, treeclosed_src = EXCLUDED.treeclosed_src
        "#,
        repo as &GithubRepoName,
        priority,
        src
    )
    .execute(executor)
    .await?;

    Ok(())
}
