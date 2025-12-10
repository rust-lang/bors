use crate::bors::event::BorsEvent;
use crate::bors::{CommandPrefix, RepositoryState, RollupMode, format_help};
use crate::database::{ApprovalStatus, PullRequestModel, QueueStatus};
use crate::github::{GithubRepoName, rollup};
use crate::templates::{
    HelpTemplate, HtmlTemplate, NotFoundTemplate, PullRequestStats, QueueTemplate, RepositoryView,
};
use crate::utils::sort_queue::sort_queue_prs;
use crate::{
    AppError, BorsGlobalEvent, BorsRepositoryEvent, PgDbClient, WebhookSecret, bors, database,
};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use http::StatusCode;
use pulldown_cmark::Parser;
use secrecy::{ExposeSecret, SecretString};
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::catch_panic::CatchPanicLayer;
use webhook::GitHubWebhook;

pub mod webhook;

#[derive(Clone)]
pub struct OAuthConfig {
    client_id: String,
    client_secret: SecretString,
}

impl OAuthConfig {
    pub fn new(client_id: String, client_secret: String) -> Self {
        Self {
            client_id,
            client_secret: client_secret.into(),
        }
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn client_secret(&self) -> &str {
        self.client_secret.expose_secret()
    }
}

/// Shared server state for all axum handlers.
pub struct ServerState {
    repository_event_queue: mpsc::Sender<BorsRepositoryEvent>,
    global_event_queue: mpsc::Sender<BorsGlobalEvent>,
    webhook_secret: WebhookSecret,
    pub(crate) oauth: Option<OAuthConfig>,
    repositories: HashMap<GithubRepoName, Arc<RepositoryState>>,
    pub(crate) db: Arc<PgDbClient>,
    cmd_prefix: CommandPrefix,
}

impl ServerState {
    pub fn new(
        repository_event_queue: mpsc::Sender<BorsRepositoryEvent>,
        global_event_queue: mpsc::Sender<BorsGlobalEvent>,
        webhook_secret: WebhookSecret,
        oauth: Option<OAuthConfig>,
        repositories: HashMap<GithubRepoName, Arc<RepositoryState>>,
        db: Arc<PgDbClient>,
        cmd_prefix: CommandPrefix,
    ) -> Self {
        Self {
            repository_event_queue,
            global_event_queue,
            webhook_secret,
            oauth,
            repositories,
            db,
            cmd_prefix,
        }
    }

    pub fn get_webhook_secret(&self) -> &WebhookSecret {
        &self.webhook_secret
    }

    pub fn get_cmd_prefix(&self) -> &CommandPrefix {
        &self.cmd_prefix
    }
}

pub type ServerStateRef = Arc<ServerState>;

pub fn create_app(state: ServerState) -> Router {
    let api = create_api_router();
    Router::new()
        .route("/", get(index_handler))
        .route("/help", get(help_handler))
        .route("/queue/{repo_name}", get(queue_handler))
        .route("/github", post(github_webhook_handler))
        .route("/health", get(health_handler))
        .route("/oauth/callback", get(rollup::oauth_callback_handler))
        .nest("/api", api)
        .layer(ConcurrencyLimitLayer::new(100))
        .layer(CatchPanicLayer::custom(handle_panic))
        .with_state(Arc::new(state))
        .fallback(not_found_handler)
}

fn create_api_router() -> Router<ServerStateRef> {
    let router = Router::new();
    router.route("/queue/{repo_name}", get(api_merge_queue))
}

async fn api_merge_queue(
    Path(repo_name): Path<String>,
    State(state): State<ServerStateRef>,
) -> Result<impl IntoResponse, AppError> {
    let repo = match state.db.repo_by_name(&repo_name).await? {
        Some(repo) => repo,
        None => {
            return Ok((
                StatusCode::NOT_FOUND,
                Json(format!("Repository {repo_name} not found")),
            )
                .into_response());
        }
    };

    #[derive(serde::Serialize)]
    #[serde(rename_all = "kebab-case")]
    enum PullRequestStatus {
        Closed,
        Draft,
        Merged,
        Open,
    }

    #[derive(serde::Serialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum BuildStatus {
        Pending,
        Success,
        Failure,
        Cancelled,
        Timeouted,
    }

    #[derive(serde::Serialize)]
    struct PullRequest {
        number: u64,
        title: String,
        author: String,
        status: PullRequestStatus,
        base_branch: String,
        priority: Option<u64>,
        approver: Option<String>,
        try_build: Option<BuildStatus>,
        auto_build: Option<BuildStatus>,
    }

    fn convert_status(status: database::BuildStatus) -> BuildStatus {
        match status {
            database::BuildStatus::Pending => BuildStatus::Pending,
            database::BuildStatus::Success => BuildStatus::Success,
            database::BuildStatus::Failure => BuildStatus::Failure,
            database::BuildStatus::Cancelled => BuildStatus::Cancelled,
            database::BuildStatus::Timeouted => BuildStatus::Timeouted,
        }
    }

    let prs = state.db.get_nonclosed_pull_requests(&repo.name).await?;
    let prs = sort_queue_prs(prs);
    let prs = prs
        .into_iter()
        .map(|pr| {
            let PullRequestModel {
                id: _,
                repository: _,
                number,
                title,
                author,
                assignees: _,
                pr_status,
                base_branch,
                mergeable_state: _,
                approval_status,
                delegated_permission: _,
                priority,
                rollup: _,
                try_build,
                auto_build,
                created_at: _,
            } = pr;
            PullRequest {
                number: number.0,
                title,
                author,
                status: match pr_status {
                    bors::PullRequestStatus::Closed => PullRequestStatus::Closed,
                    bors::PullRequestStatus::Draft => PullRequestStatus::Draft,
                    bors::PullRequestStatus::Merged => PullRequestStatus::Merged,
                    bors::PullRequestStatus::Open => PullRequestStatus::Open,
                },
                base_branch,
                priority: priority.map(|p| p as u64),
                approver: match approval_status {
                    ApprovalStatus::NotApproved => None,
                    ApprovalStatus::Approved(info) => Some(info.approver),
                },
                try_build: try_build.map(|b| convert_status(b.status)),
                auto_build: auto_build.map(|b| convert_status(b.status)),
            }
        })
        .collect::<Vec<_>>();
    Ok(Json(prs).into_response())
}

fn handle_panic(err: Box<dyn Any + Send + 'static>) -> Response {
    tracing::error!("Router panicked: {err:?}");
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
}

async fn not_found_handler() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, HtmlTemplate(NotFoundTemplate {}))
}

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, "")
}

async fn index_handler(State(state): State<ServerStateRef>) -> impl IntoResponse {
    // If we manage exactly one repo, redirect to its queue page directly
    if let Some(repo_name) = state.repositories.keys().next()
        && state.repositories.len() == 1
    {
        return Redirect::temporary(&format!("/queue/{}", repo_name.name())).into_response();
    }
    help_handler(State(state)).await.into_response()
}

async fn help_handler(State(state): State<ServerStateRef>) -> impl IntoResponse {
    let mut repos = Vec::with_capacity(state.repositories.len());
    for repo in state.repositories.keys() {
        let treeclosed = state
            .db
            .repo_db(repo)
            .await
            .ok()
            .flatten()
            .is_some_and(|repo| repo.tree_state.is_closed());
        repos.push(RepositoryView {
            name: repo.name().to_string(),
            treeclosed,
        });
    }

    let help_md = format_help();
    let markdown = Parser::new(help_md);

    let mut help_html = String::new();
    pulldown_cmark::html::push_html(&mut help_html, markdown);

    HtmlTemplate(HelpTemplate {
        repos,
        help: help_html,
        cmd_prefix: state.get_cmd_prefix().as_ref().to_string(),
    })
}

pub async fn queue_handler(
    Path(repo_name): Path<String>,
    State(state): State<ServerStateRef>,
) -> Result<impl IntoResponse, AppError> {
    let repo = match state.db.repo_by_name(&repo_name).await? {
        Some(repo) => repo,
        None => {
            return Ok((
                StatusCode::NOT_FOUND,
                format!("Repository {repo_name} not found"),
            )
                .into_response());
        }
    };

    let prs = state.db.get_nonclosed_pull_requests(&repo.name).await?;
    let prs = sort_queue_prs(prs);

    let (in_queue_count, failed_count, rolled_up_count) =
        prs.iter()
            .fold((0, 0, 0), |(in_queue, failed, rolled_up), pr| {
                let (in_queue_inc, failed_inc) = match pr.queue_status() {
                    QueueStatus::Approved(..) => (1, 0),
                    QueueStatus::ReadyForMerge(..) => (1, 0),
                    QueueStatus::Pending(..) => (1, 0),
                    QueueStatus::Stalled(..) => (0, 1),
                    QueueStatus::NotApproved => (0, 0),
                };

                (
                    in_queue + in_queue_inc,
                    failed + failed_inc,
                    rolled_up + usize::from(matches!(pr.rollup, Some(RollupMode::Always))),
                )
            });

    Ok(HtmlTemplate(QueueTemplate {
        oauth_client_id: state
            .oauth
            .as_ref()
            .map(|config| config.client_id().to_string()),
        repo_name: repo.name.name().to_string(),
        repo_owner: repo.name.owner().to_string(),
        repo_url: format!("https://github.com/{}", repo.name),
        tree_state: repo.tree_state,
        stats: PullRequestStats {
            total_count: prs.len(),
            in_queue_count,
            failed_count,
            rolled_up_count,
        },
        prs,
    })
    .into_response())
}

/// Axum handler that receives a webhook and sends it to a webhook channel.
pub async fn github_webhook_handler(
    State(state): State<ServerStateRef>,
    GitHubWebhook(event): GitHubWebhook,
) -> impl IntoResponse {
    match event {
        BorsEvent::Global(e) => match state.global_event_queue.send(e).await {
            Ok(_) => (StatusCode::OK, ""),
            Err(err) => {
                tracing::error!("Could not send webhook global event: {err:?}");
                (StatusCode::INTERNAL_SERVER_ERROR, "")
            }
        },
        BorsEvent::Repository(e) => match state.repository_event_queue.send(e).await {
            Ok(_) => (StatusCode::OK, ""),
            Err(err) => {
                tracing::error!("Could not send webhook repository event: {err:?}");
                (StatusCode::INTERNAL_SERVER_ERROR, "")
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::{BorsTester, default_repo_name, run_test};

    #[sqlx::test]
    async fn api_queue_page(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            let response = tester
                .rest_api(&format!("/api/queue/{}", default_repo_name().name()))
                .await?;
            insta::assert_snapshot!(response, @r#"[{"number":1,"title":"Title of PR 1","author":"default-user","status":"open","base_branch":"main","priority":null,"approver":"default-user","try_build":null,"auto_build":null}]"#);
            Ok(())
        })
        .await;
    }
}
