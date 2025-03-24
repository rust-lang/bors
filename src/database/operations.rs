use chrono::DateTime;
use chrono::Utc;
use sqlx::postgres::PgExecutor;

use crate::bors::PullRequestStatus;
use crate::bors::RollupMode;
use crate::database::BuildStatus;
use crate::database::RepoModel;
use crate::database::WorkflowModel;
use crate::github::CommitSha;
use crate::github::GithubRepoName;
use crate::github::PullRequestNumber;
use crate::utils::timing::measure_db_query;

use super::ApprovalInfo;
use super::ApprovalStatus;
use super::BuildModel;
use super::MergeableState;
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
    measure_db_query("get_pull_request", || async {
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
        pr.status as "pr_status: PullRequestStatus", 
        pr.priority,
        pr.rollup as "rollup: RollupMode",
        pr.delegated,
        pr.base_branch,
        pr.mergeable_state as "mergeable_state: MergeableState",
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
    })
    .await
}

pub(crate) async fn create_pull_request(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    pr_number: PullRequestNumber,
    base_branch: &str,
    pr_status: PullRequestStatus,
) -> anyhow::Result<()> {
    measure_db_query("create_pull_request", || async {
        sqlx::query!(
            r#"
INSERT INTO pull_request (repository, number, base_branch, status)
VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING
"#,
            repo as &GithubRepoName,
            pr_number.0 as i32,
            base_branch,
            pr_status as PullRequestStatus,
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
}

pub(crate) async fn set_pr_status(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    pr_number: PullRequestNumber,
    pr_status: PullRequestStatus,
) -> anyhow::Result<()> {
    measure_db_query("set_pr_status", || async {
        sqlx::query!(
            "UPDATE pull_request SET status = $3 WHERE repository = $1 AND number = $2",
            repo as &GithubRepoName,
            pr_number.0 as i32,
            pr_status as PullRequestStatus,
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
}

pub(crate) async fn upsert_pull_request(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    pr_number: PullRequestNumber,
    base_branch: &str,
    mergeable_state: MergeableState,
    pr_status: &PullRequestStatus,
) -> anyhow::Result<PullRequestModel> {
    measure_db_query("upsert_pull_request", || async {
        let record = sqlx::query_as!(
            PullRequestModel,
            r#"
            WITH upserted_pr AS (
                INSERT INTO pull_request (repository, number, base_branch, mergeable_state, status)
                VALUES ($1, $2, $3, $4, $5)
                ON CONFLICT (repository, number)
                DO UPDATE SET
                    base_branch = $3,
                    mergeable_state = $4,
                    status = $5
                RETURNING *
            )
            SELECT
                pr.id,
                pr.repository as "repository: GithubRepoName",
                pr.number as "number!: i64",
                (
                    pr.approved_by,
                    pr.approved_sha
                ) AS "approval_status!: ApprovalStatus",
                pr.status as "pr_status: PullRequestStatus", 
                pr.priority,
                pr.rollup as "rollup: RollupMode",
                pr.delegated,
                pr.base_branch,
                pr.mergeable_state as "mergeable_state: MergeableState",
                pr.created_at as "created_at: DateTime<Utc>",
                build AS "try_build: BuildModel"
            FROM upserted_pr as pr
            LEFT JOIN build ON pr.build_id = build.id
            "#,
            repo as &GithubRepoName,
            pr_number.0 as i32,
            base_branch,
            mergeable_state as _,
            pr_status as &PullRequestStatus,
        )
        .fetch_one(executor)
        .await?;
        Ok(record)
    })
    .await
}

// FIXME:
// 1) Add a database index on (repository, base_branch)
// 2) Filter PRs by state (only update open PRs, once we have PR state tracking)
pub(crate) async fn update_mergeable_states_by_base_branch(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    base_branch: &str,
    mergeable_state: MergeableState,
) -> anyhow::Result<u64> {
    measure_db_query("update_mergeable_states_by_base_branch", || async {
        let result = sqlx::query!(
            r#"
            UPDATE pull_request
            SET mergeable_state = $1
            WHERE repository = $2 AND base_branch = $3
            "#,
            mergeable_state as _,
            repo as &GithubRepoName,
            base_branch,
        )
        .execute(executor)
        .await?;

        Ok(result.rows_affected())
    })
    .await
}

pub(crate) async fn approve_pull_request(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
    approval_info: ApprovalInfo,
    priority: Option<u32>,
    rollup: Option<RollupMode>,
) -> anyhow::Result<()> {
    let priority_i32 = priority.map(|p| p as i32);

    measure_db_query("approve_pull_request", || async {
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
            rollup as Option<RollupMode>,
            pr_id,
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
}

pub(crate) async fn unapprove_pull_request(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
) -> anyhow::Result<()> {
    measure_db_query("unapprove_pull_request", || async {
        sqlx::query!(
            "UPDATE pull_request SET approved_by = NULL, approved_sha = NULL WHERE id = $1",
            pr_id
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
}

pub(crate) async fn delegate_pull_request(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
) -> anyhow::Result<()> {
    measure_db_query("delegate_pull_request", || async {
        sqlx::query!(
            "UPDATE pull_request SET delegated = TRUE WHERE id = $1",
            pr_id,
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
}

pub(crate) async fn undelegate_pull_request(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
) -> anyhow::Result<()> {
    measure_db_query("undelegate_pull_request", || async {
        sqlx::query!(
            "UPDATE pull_request SET delegated = FALSE WHERE id = $1",
            pr_id
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
}

pub(crate) async fn find_pr_by_build(
    executor: impl PgExecutor<'_>,
    build_id: i32,
) -> anyhow::Result<Option<PullRequestModel>> {
    measure_db_query("find_pr_by_build", || async {
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
    pr.status as "pr_status: PullRequestStatus",  
    pr.delegated,
    pr.priority,
    pr.base_branch,
    pr.mergeable_state as "mergeable_state: MergeableState",
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
    })
    .await
}

pub(crate) async fn update_pr_build_id(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
    build_id: i32,
) -> anyhow::Result<()> {
    measure_db_query("update_pr_build_id", || async {
        sqlx::query!(
            "UPDATE pull_request SET build_id = $1 WHERE id = $2",
            build_id,
            pr_id
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
}

pub(crate) async fn create_build(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    branch: &str,
    commit_sha: &CommitSha,
    parent: &CommitSha,
) -> anyhow::Result<i32> {
    measure_db_query("create_build", || async {
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
    })
    .await
}

pub(crate) async fn find_build(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    branch: &str,
    commit_sha: &CommitSha,
) -> anyhow::Result<Option<BuildModel>> {
    measure_db_query("find_build", || async {
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
    })
    .await
}

pub(crate) async fn get_running_builds(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
) -> anyhow::Result<Vec<BuildModel>> {
    measure_db_query("get_running_builds", || async {
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
    })
    .await
}

pub(crate) async fn update_build_status(
    executor: impl PgExecutor<'_>,
    build_id: i32,
    status: BuildStatus,
) -> anyhow::Result<()> {
    measure_db_query("update_build_status", || async {
        sqlx::query!(
            "UPDATE build SET status = $1 WHERE id = $2",
            status as _,
            build_id
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
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
    measure_db_query("create_workflow", || async {
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
    })
    .await
}

pub(crate) async fn update_workflow_status(
    executor: impl PgExecutor<'_>,
    run_id: u64,
    status: WorkflowStatus,
) -> anyhow::Result<()> {
    measure_db_query("update_workflow_status", || async {
        sqlx::query!(
            "UPDATE workflow SET status = $1 WHERE run_id = $2",
            status as _,
            run_id as i64
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
}

pub(crate) async fn set_pr_priority(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
    priority: u32,
) -> anyhow::Result<()> {
    measure_db_query("set_pr_priority", || async {
        sqlx::query!(
            "UPDATE pull_request SET priority = $1 WHERE id = $2",
            priority as i32,
            pr_id,
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
}

pub(crate) async fn set_pr_rollup(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
    rollup: RollupMode,
) -> anyhow::Result<()> {
    measure_db_query("set_pr_rollup", || async {
        sqlx::query!(
            "UPDATE pull_request SET rollup = $1 WHERE id = $2",
            rollup as RollupMode,
            pr_id,
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
}

pub(crate) async fn get_workflows_for_build(
    executor: impl PgExecutor<'_>,
    build_id: i32,
) -> anyhow::Result<Vec<WorkflowModel>> {
    measure_db_query("get_workflows_for_build", || async {
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
    })
    .await
}

pub(crate) async fn get_workflow_urls_for_build(
    executor: impl PgExecutor<'_>,
    build_id: i32,
) -> anyhow::Result<Vec<String>> {
    measure_db_query("get_workflow_urls_for_build", || async {
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
    })
    .await
}

#[cfg(test)]
pub(crate) async fn get_all_workflows(
    executor: impl PgExecutor<'_>,
) -> anyhow::Result<Vec<WorkflowModel>> {
    measure_db_query("get_all_workflows", || async {
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
    })
    .await
}

/// Retrieves a repository from the database or creates it if it does not exist.
pub(crate) async fn get_repository(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
) -> anyhow::Result<Option<RepoModel>> {
    measure_db_query("get_repository", || async {
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
    })
    .await
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
    measure_db_query("upsert_repository", || async {
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
    })
    .await
}
