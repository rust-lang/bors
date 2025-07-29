use chrono::DateTime;
use chrono::Utc;
use sqlx::postgres::PgExecutor;

use crate::bors::PullRequestStatus;
use crate::bors::RollupMode;
use crate::bors::comment::CommentTag;
use crate::database::BuildStatus;
use crate::database::RepoModel;
use crate::database::WorkflowModel;
use crate::github::CommitSha;
use crate::github::GithubRepoName;
use crate::github::PullRequestNumber;
use crate::utils::timing::measure_db_query;

use super::ApprovalInfo;
use super::ApprovalStatus;
use super::Assignees;
use super::BuildModel;
use super::CommentModel;
use super::DelegatedPermission;
use super::MergeableState;
use super::PullRequestModel;
use super::RunId;
use super::TreeState;
use super::UpsertPullRequestParams;
use super::WorkflowStatus;
use super::WorkflowType;
use futures::TryStreamExt;

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
        pr.title,
        pr.author,
        pr.assignees as "assignees: Assignees",
        (
            pr.approved_by,
            pr.approved_sha
        ) AS "approval_status!: ApprovalStatus",
        pr.status as "pr_status: PullRequestStatus",
        pr.priority,
        pr.rollup as "rollup: RollupMode",
        pr.delegated_permission as "delegated_permission: DelegatedPermission",
        pr.base_branch,
        pr.mergeable_state as "mergeable_state: MergeableState",
        pr.created_at as "created_at: DateTime<Utc>",
        try_build AS "try_build: BuildModel",
        auto_build AS "auto_build: BuildModel"
    FROM pull_request as pr
    LEFT JOIN build AS try_build ON pr.try_build_id = try_build.id
    LEFT JOIN build AS auto_build ON pr.auto_build_id = auto_build.id
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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn create_pull_request(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    pr_number: PullRequestNumber,
    title: &str,
    author: &str,
    assignees: &[String],
    base_branch: &str,
    pr_status: PullRequestStatus,
) -> anyhow::Result<()> {
    measure_db_query("create_pull_request", || async {
        sqlx::query!(
            r#"
INSERT INTO pull_request (repository, number, title, author, assignees, base_branch, status)
VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING
"#,
            repo as &GithubRepoName,
            pr_number.0 as i32,
            title,
            author,
            assignees.join(","),
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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn upsert_pull_request(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    params: &UpsertPullRequestParams,
) -> anyhow::Result<PullRequestModel> {
    measure_db_query("upsert_pull_request", || async {
        let record = sqlx::query_as!(
            PullRequestModel,
            r#"
            WITH upserted_pr AS (
                INSERT INTO pull_request (repository, number, title, author, assignees, base_branch, mergeable_state, status)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                ON CONFLICT (repository, number)
                DO UPDATE SET
                    title = $3,
                    author = $4,
                    assignees = $5,
                    base_branch = $6,
                    mergeable_state = $7,
                    status = $8
                RETURNING *
            )
            SELECT
                pr.id,
                pr.repository as "repository: GithubRepoName",
                pr.number as "number!: i64",
                pr.title,
                pr.author,
                pr.assignees as "assignees: Assignees",
                (
                    pr.approved_by,
                    pr.approved_sha
                ) AS "approval_status!: ApprovalStatus",
                pr.status as "pr_status: PullRequestStatus",
                pr.priority,
                pr.rollup as "rollup: RollupMode",
                pr.delegated_permission as "delegated_permission: DelegatedPermission",
                pr.base_branch,
                pr.mergeable_state as "mergeable_state: MergeableState",
                pr.created_at as "created_at: DateTime<Utc>",
                try_build AS "try_build: BuildModel",
                auto_build AS "auto_build: BuildModel"
            FROM upserted_pr as pr
            LEFT JOIN build AS try_build ON pr.try_build_id = try_build.id
            LEFT JOIN build AS auto_build ON pr.auto_build_id = auto_build.id
            "#,
            repo as &GithubRepoName,
            params.pr_number.0 as i32,
            &params.title,
            &params.author,
            params.assignees.join(","),
            &params.base_branch,
            params.mergeable_state as _,
            params.pr_status as _,
        )
        .fetch_one(executor)
        .await?;
        Ok(record)
    })
    .await
}

/// Uses inclusion rather than negation, which would cause a full table scan,
/// to leverage the index from PR #246 (https://github.com/rust-lang/bors/pull/246).
pub(crate) async fn get_nonclosed_pull_requests_by_base_branch(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    base_branch: &str,
) -> anyhow::Result<Vec<PullRequestModel>> {
    measure_db_query("get_pull_requests_by_base_branch", || async {
        let records = sqlx::query_as!(
            PullRequestModel,
            r#"
            SELECT
                pr.id,
                pr.repository as "repository: GithubRepoName",
                pr.number as "number!: i64",
                pr.title,
                pr.author,
                pr.assignees as "assignees: Assignees",
                (
                    pr.approved_by,
                    pr.approved_sha
                ) AS "approval_status!: ApprovalStatus",
                pr.status as "pr_status: PullRequestStatus",
                pr.priority,
                pr.rollup as "rollup: RollupMode",
                pr.delegated_permission as "delegated_permission: DelegatedPermission",
                pr.base_branch,
                pr.mergeable_state as "mergeable_state: MergeableState",
                pr.created_at as "created_at: DateTime<Utc>",
                try_build AS "try_build: BuildModel",
                auto_build AS "auto_build: BuildModel"
            FROM pull_request as pr
            LEFT JOIN build AS try_build ON pr.try_build_id = try_build.id
            LEFT JOIN build AS auto_build ON pr.auto_build_id = auto_build.id
            WHERE pr.repository = $1
              AND pr.base_branch = $2
              AND pr.status IN ('open', 'draft')
            "#,
            repo as &GithubRepoName,
            base_branch
        )
        .fetch_all(executor)
        .await?;

        Ok(records)
    })
    .await
}

pub(crate) async fn get_nonclosed_pull_requests(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
) -> anyhow::Result<Vec<PullRequestModel>> {
    measure_db_query("fetch_pull_requests", || async {
        let mut stream = sqlx::query_as!(
            PullRequestModel,
            r#"
            SELECT
                pr.id,
                pr.repository as "repository: GithubRepoName",
                pr.number as "number!: i64",
                pr.title,
                pr.author,
                pr.assignees as "assignees: Assignees",
                (
                    pr.approved_by,
                    pr.approved_sha
                ) AS "approval_status!: ApprovalStatus",
                pr.status as "pr_status: PullRequestStatus",
                pr.priority,
                pr.rollup as "rollup: RollupMode",
                pr.delegated_permission as "delegated_permission: DelegatedPermission",
                pr.base_branch,
                pr.mergeable_state as "mergeable_state: MergeableState",
                pr.created_at as "created_at: DateTime<Utc>",
                try_build AS "try_build: BuildModel",
                auto_build AS "auto_build: BuildModel"
            FROM pull_request as pr
            LEFT JOIN build AS try_build ON pr.try_build_id = try_build.id
            LEFT JOIN build AS auto_build ON pr.auto_build_id = auto_build.id
            WHERE pr.repository = $1
                AND pr.status IN ('open', 'draft')
            "#,
            repo as &GithubRepoName
        )
        .fetch(executor);
        let mut prs = Vec::new();
        while let Some(pr) = stream.try_next().await? {
            prs.push(pr);
        }
        Ok(prs)
    })
    .await
}

pub(crate) async fn update_pr_mergeable_state(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
    mergeable_state: MergeableState,
) -> anyhow::Result<()> {
    measure_db_query("update_pr_mergeable_state", || async {
        sqlx::query!(
            "UPDATE pull_request SET mergeable_state = $1 WHERE id = $2",
            mergeable_state as _,
            pr_id
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
}

pub(crate) async fn get_prs_with_unknown_mergeable_state(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
) -> anyhow::Result<Vec<PullRequestModel>> {
    measure_db_query("get_prs_with_unknown_mergeable_state", || async {
        let records = sqlx::query_as!(
            PullRequestModel,
            r#"
            SELECT
                pr.id,
                pr.repository as "repository: GithubRepoName",
                pr.number as "number!: i64",
                pr.title,
                pr.author,
                pr.assignees as "assignees: Assignees",
                (
                    pr.approved_by,
                    pr.approved_sha
                ) AS "approval_status!: ApprovalStatus",
                pr.status as "pr_status: PullRequestStatus",
                pr.priority,
                pr.rollup as "rollup: RollupMode",
                pr.delegated_permission as "delegated_permission: DelegatedPermission",
                pr.base_branch,
                pr.mergeable_state as "mergeable_state: MergeableState",
                pr.created_at as "created_at: DateTime<Utc>",
                try_build AS "try_build: BuildModel",
                auto_build AS "auto_build: BuildModel"
            FROM pull_request as pr
            LEFT JOIN build AS try_build ON pr.try_build_id = try_build.id
            LEFT JOIN build AS auto_build ON pr.auto_build_id = auto_build.id
            WHERE pr.repository = $1
              AND pr.mergeable_state = 'unknown'
              AND pr.status IN ('open', 'draft')
            "#,
            repo as &GithubRepoName
        )
        .fetch_all(executor)
        .await?;

        Ok(records)
    })
    .await
}

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
            WHERE repository = $2
            AND base_branch = $3
            AND status IN ('open', 'draft')
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
    delegated_permission: DelegatedPermission,
) -> anyhow::Result<()> {
    measure_db_query("delegate_pull_request", || async {
        sqlx::query!(
            "UPDATE pull_request SET delegated_permission = $1 WHERE id = $2",
            delegated_permission as _,
            pr_id
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
            "UPDATE pull_request SET delegated_permission = NULL WHERE id = $1",
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
    pr.title,
    pr.author,
    pr.assignees as "assignees: Assignees",
    (
        pr.approved_by,
        pr.approved_sha
    ) AS "approval_status!: ApprovalStatus",
    pr.status as "pr_status: PullRequestStatus",
    pr.delegated_permission as "delegated_permission: DelegatedPermission",
    pr.priority,
    pr.base_branch,
    pr.mergeable_state as "mergeable_state: MergeableState",
    pr.rollup as "rollup: RollupMode",
    pr.created_at as "created_at: DateTime<Utc>",
    try_build AS "try_build: BuildModel",
    auto_build AS "auto_build: BuildModel"
FROM pull_request as pr
LEFT JOIN build AS try_build ON pr.try_build_id = try_build.id
LEFT JOIN build AS auto_build ON pr.auto_build_id = auto_build.id
WHERE try_build.id = $1 OR auto_build.id = $1
"#,
            build_id
        )
        .fetch_optional(executor)
        .await?;

        Ok(record)
    })
    .await
}

pub(crate) async fn update_pr_try_build_id(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
    try_build_id: i32,
) -> anyhow::Result<()> {
    measure_db_query("update_pr_try_build_id", || async {
        sqlx::query!(
            "UPDATE pull_request SET try_build_id = $1 WHERE id = $2",
            try_build_id,
            pr_id
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
}

pub(crate) async fn update_pr_auto_build_id(
    executor: impl PgExecutor<'_>,
    pr_id: i32,
    auto_build_id: i32,
) -> anyhow::Result<()> {
    measure_db_query("update_pr_auto_build_id", || async {
        sqlx::query!(
            "UPDATE pull_request SET auto_build_id = $1 WHERE id = $2",
            auto_build_id,
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
    status as "status: BuildStatus",
    parent,
    created_at as "created_at: DateTime<Utc>",
    check_run_id
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

pub(crate) async fn get_pending_builds(
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
    status as "status: BuildStatus",
    parent,
    created_at as "created_at: DateTime<Utc>",
    check_run_id
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

pub(crate) async fn set_pr_assignees(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    pr_number: PullRequestNumber,
    assignees: &[String],
) -> anyhow::Result<()> {
    measure_db_query("set_pr_assignees", || async {
        sqlx::query!(
            "UPDATE pull_request SET assignees = $1 WHERE repository = $2 AND number = $3",
            assignees.join(","),
            repo as &GithubRepoName,
            pr_number.0 as i32,
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
        build.created_at,
        build.check_run_id
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
        build.created_at,
        build.check_run_id
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

pub(crate) async fn insert_repo_if_not_exists(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    tree_state: TreeState,
) -> anyhow::Result<()> {
    let (priority, src) = match tree_state {
        TreeState::Open => (None, None),
        TreeState::Closed { priority, source } => (Some(priority as i32), Some(source)),
    };
    measure_db_query("insert_repository_if_not_exists", || async {
        sqlx::query!(
            r#"
        INSERT INTO repository (name, tree_state, treeclosed_src)
        VALUES ($1, $2, $3)
        ON CONFLICT (name) DO NOTHING
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

/// Returns the first match found for a repository by name without owner (via `/{repo_name}`).
pub(crate) async fn get_repository_by_name(
    executor: impl PgExecutor<'_>,
    repo_name: &str,
) -> anyhow::Result<Option<RepoModel>> {
    measure_db_query("get_repository_by_name", || async {
        let search_pattern = format!("%/{repo_name}");
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
        WHERE name LIKE $1
        LIMIT 1
        "#,
            search_pattern
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

pub(crate) async fn update_build_check_run_id(
    executor: impl PgExecutor<'_>,
    build_id: i32,
    check_run_id: i64,
) -> anyhow::Result<()> {
    measure_db_query("update_build_check_run_id", || async {
        sqlx::query!(
            "UPDATE build SET check_run_id = $1 WHERE id = $2",
            check_run_id,
            build_id
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
}

/// Fetches pull requests eligible for merge:
/// - Only approved PRs that are open and mergeable
/// - Includes only PRs with pending or successful auto builds
/// - Excludes PRs that do not meet the tree closure priority threshold (if tree closed)
pub(crate) async fn get_merge_queue_prs(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    tree_priority: Option<i32>,
) -> anyhow::Result<Vec<PullRequestModel>> {
    measure_db_query("get_merge_queue_prs", || async {
        let records = sqlx::query_as!(
            PullRequestModel,
            r#"
            SELECT
                pr.id,
                pr.repository as "repository: GithubRepoName",
                pr.number as "number!: i64",
                pr.title,
                pr.author,
                pr.assignees as "assignees: Assignees",
                (
                    pr.approved_by,
                    pr.approved_sha
                ) AS "approval_status!: ApprovalStatus",
                pr.status as "pr_status: PullRequestStatus",
                pr.priority,
                pr.rollup as "rollup: RollupMode",
                pr.delegated_permission as "delegated_permission: DelegatedPermission",
                pr.base_branch,
                pr.mergeable_state as "mergeable_state: MergeableState",
                pr.created_at as "created_at: DateTime<Utc>",
                try_build AS "try_build: BuildModel",
                auto_build AS "auto_build: BuildModel"
            FROM pull_request as pr
            LEFT JOIN build AS try_build ON pr.try_build_id = try_build.id
            LEFT JOIN build AS auto_build ON pr.auto_build_id = auto_build.id
            WHERE pr.repository = $1
              AND pr.status = 'open'
              AND pr.approved_by IS NOT NULL
              AND pr.mergeable_state = 'mergeable'
              -- Include only PRs with pending or successful auto builds
              AND (auto_build.status IS NULL OR auto_build.status IN ('pending', 'success'))
              -- Tree closure check (if tree_priority is set)
              AND ($2::int IS NULL OR pr.priority >= $2)
            "#,
            repo as &GithubRepoName,
            tree_priority
        )
        .fetch_all(executor)
        .await?;
        Ok(records)
    })
    .await
}

pub(crate) async fn get_comments(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    pr_number: PullRequestNumber,
    tag: CommentTag,
) -> anyhow::Result<Vec<CommentModel>> {
    measure_db_query("get_comments", || async {
        let comments = sqlx::query_as!(
            CommentModel,
            r#"
            SELECT
                id,
                repository as "repository: GithubRepoName",
                pr_number as "pr_number: i64",
                tag as "tag: CommentTag",
                node_id,
                created_at as "created_at: DateTime<Utc>"
            FROM comment
            WHERE repository = $1
              AND pr_number = $2
              AND tag = $3
            "#,
            repo as &GithubRepoName,
            pr_number.0 as i32,
            tag as CommentTag,
        )
        .fetch_all(executor)
        .await?;
        Ok(comments)
    })
    .await
}

pub(crate) async fn insert_comment(
    executor: impl PgExecutor<'_>,
    repo: &GithubRepoName,
    pr_number: PullRequestNumber,
    tag: CommentTag,
    node_id: &str,
) -> anyhow::Result<()> {
    measure_db_query("insert_comment", || async {
        sqlx::query!(
            r#"
            INSERT INTO comment (repository, pr_number, tag, node_id)
            VALUES ($1, $2, $3, $4)
            "#,
            repo as &GithubRepoName,
            pr_number.0 as i32,
            tag as CommentTag,
            node_id
        )
        .execute(executor)
        .await?;
        Ok(())
    })
    .await
}

pub(crate) async fn delete_comment(executor: impl PgExecutor<'_>, id: i32) -> anyhow::Result<()> {
    measure_db_query("delete_comment", || async {
        sqlx::query!("DELETE FROM comment WHERE id = $1", id)
            .execute(executor)
            .await?;
        Ok(())
    })
    .await
}
