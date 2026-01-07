use crate::bors::event::BorsEvent;
use crate::bors::{CommandPrefix, RepositoryState, format_help};
use crate::database::{ApprovalStatus, PullRequestModel, QueueStatus};
use crate::github::{GithubRepoName, rollup};
use crate::templates::{
    HelpTemplate, HtmlTemplate, NotFoundTemplate, PullRequestStats, QueueTemplate, RepositoryView,
};
use crate::utils::sort_queue::sort_queue_prs;
use crate::{
    AppError, BorsContext, BorsGlobalEvent, BorsRepositoryEvent, OAuthClient, PgDbClient,
    WebhookSecret, bors, database,
};
use axum::extract::{FromRef, Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use http::StatusCode;
use pulldown_cmark::Parser;
use serde::de::Error;
use serde::{Deserialize, Deserializer};
use std::any::Any;
use std::sync::Arc;
use tokio::sync::mpsc;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use webhook::GitHubWebhook;

pub mod webhook;

/// Shared server state for all axum handlers.
pub struct ServerState {
    repository_event_queue: mpsc::Sender<BorsRepositoryEvent>,
    global_event_queue: mpsc::Sender<BorsGlobalEvent>,
    webhook_secret: WebhookSecret,
    oauth: Option<OAuthClient>,
    ctx: Arc<BorsContext>,
}

impl ServerState {
    pub fn new(
        repository_event_queue: mpsc::Sender<BorsRepositoryEvent>,
        global_event_queue: mpsc::Sender<BorsGlobalEvent>,
        webhook_secret: WebhookSecret,
        oauth: Option<OAuthClient>,
        ctx: Arc<BorsContext>,
    ) -> Self {
        Self {
            repository_event_queue,
            global_event_queue,
            webhook_secret,
            oauth,
            ctx,
        }
    }

    pub fn get_webhook_secret(&self) -> &WebhookSecret {
        &self.webhook_secret
    }

    pub fn get_cmd_prefix(&self) -> &CommandPrefix {
        self.ctx.parser.prefix()
    }

    pub fn get_web_url(&self) -> &str {
        self.ctx.get_web_url()
    }

    pub fn get_repo(&self, repo: &GithubRepoName) -> Option<Arc<RepositoryState>> {
        self.ctx.repositories.get(repo)
    }
}

impl FromRef<ServerStateRef> for Option<OAuthClient> {
    fn from_ref(state: &ServerStateRef) -> Self {
        state.0.oauth.clone()
    }
}

impl FromRef<ServerStateRef> for Arc<PgDbClient> {
    fn from_ref(state: &ServerStateRef) -> Self {
        state.0.ctx.db.clone()
    }
}

#[derive(Clone)]
pub struct ServerStateRef(pub Arc<ServerState>);

pub fn create_app(state: ServerState) -> Router {
    let compression_layer = CompressionLayer::new().br(true).gzip(true);

    let api = create_api_router();
    Router::new()
        .route("/", get(index_handler))
        .route("/help", get(help_handler))
        .route(
            "/queue/{repo_name}",
            get(queue_handler).layer(compression_layer),
        )
        .route("/github", post(github_webhook_handler))
        .route("/health", get(health_handler))
        .route("/oauth/callback", get(rollup::oauth_callback_handler))
        .nest("/api", api)
        .layer(ConcurrencyLimitLayer::new(100))
        .layer(CatchPanicLayer::custom(handle_panic))
        .with_state(ServerStateRef(Arc::new(state)))
        .fallback(not_found_handler)
}

fn create_api_router() -> Router<ServerStateRef> {
    let router = Router::new();
    router.route("/queue/{repo_name}", get(api_merge_queue))
}

async fn api_merge_queue(
    Path(repo_name): Path<String>,
    State(db): State<Arc<PgDbClient>>,
) -> Result<impl IntoResponse, AppError> {
    let repo = match db.repo_by_name(&repo_name).await? {
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
        head_branch: String,
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

    let prs = db.get_nonclosed_pull_requests(&repo.name).await?;
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
                head_branch,
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
                head_branch,
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

async fn index_handler(State(ServerStateRef(state)): State<ServerStateRef>) -> impl IntoResponse {
    // If we manage exactly one repo, redirect to its queue page directly
    if let Some(repo_name) = state.ctx.repositories.repository_names().pop()
        && state.ctx.repositories.repo_count() == 1
    {
        return Redirect::temporary(&format!("/queue/{}", repo_name.name())).into_response();
    };
    help_handler(State(ServerStateRef(state)))
        .await
        .into_response()
}

async fn help_handler(State(ServerStateRef(state)): State<ServerStateRef>) -> impl IntoResponse {
    let mut repos = Vec::with_capacity(state.ctx.repositories.repo_count());
    for repo in state.ctx.repositories.repository_names() {
        let treeclosed = state
            .ctx
            .db
            .repo_db(&repo)
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

#[derive(serde::Deserialize)]
pub struct QueueParams {
    #[serde(rename = "prs")]
    pull_requests: Option<PullRequestList>,
}

pub struct PullRequestList(Vec<u32>);

impl<'de> Deserialize<'de> for PullRequestList {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let prs = <&str>::deserialize(deserializer)?;
        let prs = prs
            .split(",")
            .map(|pr| {
                pr.parse::<u32>()
                    .map_err(|e| D::Error::custom(e.to_string()))
            })
            .collect::<Result<Vec<u32>, D::Error>>()?;
        Ok(Self(prs))
    }
}

pub async fn queue_handler(
    Path(repo_name): Path<String>,
    State(db): State<Arc<PgDbClient>>,
    State(oauth): State<Option<OAuthClient>>,
    Query(params): Query<QueueParams>,
) -> Result<impl IntoResponse, AppError> {
    let repo = match db.repo_by_name(&repo_name).await? {
        Some(repo) => repo,
        None => {
            return Ok((
                StatusCode::NOT_FOUND,
                format!("Repository {repo_name} not found"),
            )
                .into_response());
        }
    };

    let prs = db.get_nonclosed_pull_requests(&repo.name).await?;
    let prs = sort_queue_prs(prs);

    // Note: this assumed that there is ever at most a single pending build
    let pending_build = prs.iter().find_map(|pr| match pr.queue_status() {
        QueueStatus::Pending(_, build) => Some(build),
        _ => None,
    });
    let pending_workflow = match pending_build {
        Some(build) => db.get_workflows_for_build(&build).await?.into_iter().next(),
        None => None,
    };

    let (in_queue_count, failed_count) = prs.iter().fold((0, 0), |(in_queue, failed), pr| {
        let (in_queue_inc, failed_inc) = match pr.queue_status() {
            QueueStatus::Approved(..) => (1, 0),
            QueueStatus::ReadyForMerge(..) => (1, 0),
            QueueStatus::Pending(..) => (1, 0),
            QueueStatus::Failed(..) => (0, 1),
            QueueStatus::NotApproved => (0, 0),
        };

        (in_queue + in_queue_inc, failed + failed_inc)
    });

    Ok(HtmlTemplate(QueueTemplate {
        oauth_client_id: oauth
            .as_ref()
            .map(|client| client.config().client_id().to_string()),
        repo_name: repo.name.name().to_string(),
        repo_owner: repo.name.owner().to_string(),
        repo_url: format!("https://github.com/{}", repo.name),
        tree_state: repo.tree_state,
        stats: PullRequestStats {
            total_count: prs.len(),
            in_queue_count,
            failed_count,
        },
        prs,
        pending_workflow,
        selected_rollup_prs: params.pull_requests.map(|prs| prs.0).unwrap_or_default(),
    })
    .into_response())
}

/// Axum handler that receives a webhook and sends it to a webhook channel.
pub async fn github_webhook_handler(
    State(ServerStateRef(state)): State<ServerStateRef>,
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
    use crate::tests::{ApiRequest, default_repo_name};
    use crate::tests::{BorsTester, run_test};

    #[sqlx::test]
    async fn api_queue_page(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            let response = ctx
                .api_request(ApiRequest::get(&format!("/api/queue/{}", default_repo_name().name())))
                .await?
                .assert_ok()
                .into_body();
            insta::assert_snapshot!(response, @r#"[{"number":1,"title":"Title of PR 1","author":"default-user","status":"open","head_branch":"pr-1","base_branch":"main","priority":null,"approver":"default-user","try_build":null,"auto_build":null}]"#);
            Ok(())
        })
        .await;
    }
}
