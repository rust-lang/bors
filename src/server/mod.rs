use crate::bors::event::BorsEvent;
use crate::bors::{CommandPrefix, RepositoryState, format_help};
use crate::database::{ApprovalStatus, QueueStatus};
use crate::github::{GitHubSession, GithubRepoName, OAuthExchangeCode, PullRequestNumber, rollup};
use crate::templates::{
    HelpTemplate, HtmlTemplate, NotFoundTemplate, PullRequestStats, QueueTemplate, RepositoryView,
    RollupsInfo,
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
use axum_embed::ServeEmbed;
use axum_session::{SessionConfig, SessionLayer, SessionNullSession, SessionNullSessionStore};
use base64::Engine;
use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use chrono::Utc;
use http::{Request, StatusCode};
use pulldown_cmark::Parser;
use rust_embed::Embed;
use serde::de::Error;
use serde::{Deserialize, Deserializer};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::CompressionLevel;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;
use tracing::Span;
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

pub async fn create_app(state: ServerState, insecure_cookies: bool) -> anyhow::Result<Router> {
    let compression_layer = CompressionLayer::new()
        .br(true)
        .gzip(true)
        // The production bors machine is relatively weak, prefer faster compression, rather than
        // minimum file sizes
        .quality(CompressionLevel::Fastest);
    let trace_layer = TraceLayer::new_for_http()
        .make_span_with(|request: &Request<_>| {
            tracing::debug_span!("request", "{} {}", request.method(), request.uri().path())
        })
        .on_request(())
        .on_body_chunk(())
        .on_eos(())
        .on_failure(())
        .on_response(
            |response: &http::Response<_>, latency: Duration, _span: &Span| {
                tracing::debug!(
                    "response: {} ({}ms)",
                    response.status().as_u16(),
                    latency.as_millis()
                )
            },
        );

    if insecure_cookies {
        tracing::warn!("Secure cookies are disabled. This is only recommended in local dev");
    }

    let session_config = SessionConfig::default()
        .with_http_only(true)
        .with_key(axum_session::Key::generate())
        .with_memory_lifetime(chrono::Duration::hours(2))
        .with_secure(!insecure_cookies)
        .with_table_name("web_sessions");
    let session_store = SessionNullSessionStore::new(None, session_config).await?;

    #[derive(Embed, Clone)]
    #[folder = "web/assets/"]
    struct Assets;

    let serve_assets = ServeEmbed::<Assets>::new();

    let api = create_api_router();
    Ok(Router::new()
        .route("/", get(index_handler))
        .route("/help", get(help_handler))
        .route(
            "/queue/{repo_owner}/{repo_name}",
            get(queue_handler).layer(compression_layer),
        )
        .route("/github", post(github_webhook_handler))
        .route("/health", get(health_handler))
        .route("/oauth/callback", get(oauth_callback_handler))
        .route("/rollup/submit", get(rollup::submit))
        .route("/logout", get(logout_handler))
        .nest("/api", api)
        .nest_service("/assets", serve_assets)
        .layer(SessionLayer::new(session_store))
        .layer(ConcurrencyLimitLayer::new(100))
        .layer(CatchPanicLayer::custom(handle_panic))
        .layer(trace_layer)
        .with_state(ServerStateRef(Arc::new(state)))
        .fallback(not_found_handler))
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
        .map(|pr| PullRequest {
            number: pr.number.0,
            title: pr.title,
            author: pr.author,
            status: match pr.status {
                bors::PullRequestStatus::Closed => PullRequestStatus::Closed,
                bors::PullRequestStatus::Draft => PullRequestStatus::Draft,
                bors::PullRequestStatus::Merged => PullRequestStatus::Merged,
                bors::PullRequestStatus::Open => PullRequestStatus::Open,
            },
            head_branch: pr.head_branch,
            base_branch: pr.base_branch,
            priority: pr.priority.map(|p| p as u64),
            approver: match pr.approval_status {
                ApprovalStatus::NotApproved => None,
                ApprovalStatus::Approved(info) => Some(info.approver),
            },
            try_build: pr.try_build.map(|b| convert_status(b.status)),
            auto_build: pr.auto_build.map(|b| convert_status(b.status)),
        })
        .collect::<Vec<_>>();
    Ok(Json(prs).into_response())
}

fn handle_panic(_err: Box<dyn Any + Send + 'static>) -> Response {
    tracing::error!("Router panicked");
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
}

async fn not_found_handler() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        HtmlTemplate(NotFoundTemplate {
            login_url: None,
            github_user: None,
        }),
    )
}

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, "")
}

async fn index_handler(
    session: SessionNullSession,
    State(ServerStateRef(state)): State<ServerStateRef>,
    State(oauth): State<Option<OAuthClient>>,
) -> impl IntoResponse {
    // If we manage exactly one repo, redirect to its queue page directly
    if let Some(repo) = state.ctx.repositories.repositories().pop()
        && state.ctx.repositories.repo_count() == 1
    {
        let repo_name = repo.repository();
        let mut is_visible = !repo.private;
        if !is_visible && let Some(gh_session) = GitHubSession::restore(&session) {
            let oauth_client = oauth.as_ref().unwrap();
            // if there's an error with determining if the repo is visible, then fall back to not redirecting
            match oauth_client.get_authenticated_client(&gh_session.access_token) {
                Ok(authenticated_client) => {
                    match repo_name.is_visible_to_client(&authenticated_client).await {
                        Ok(visibility) => is_visible = visibility,
                        Err(error) => tracing::warn!("{error}"),
                    }
                }
                Err(error) => tracing::warn!("{error}"),
            }
        }

        if is_visible {
            return Redirect::temporary(&format!("/queue/{repo_name}")).into_response();
        }
    }
    help_handler(session, State(ServerStateRef(state)), State(oauth))
        .await
        .into_response()
}

async fn help_handler(
    session: SessionNullSession,
    State(ServerStateRef(state)): State<ServerStateRef>,
    State(oauth): State<Option<OAuthClient>>,
) -> impl IntoResponse {
    let github_session = GitHubSession::restore(&session);
    let authenticated_client = match github_session.as_ref() {
        None => None,
        Some(gh_session) => {
            let oauth_client = oauth.as_ref().unwrap();
            match oauth_client.get_authenticated_client(&gh_session.access_token) {
                Ok(client) => Some(client),
                Err(error) => {
                    tracing::warn!("{error}");
                    None
                }
            }
        }
    };

    let mut repos = Vec::with_capacity(state.ctx.repositories.repo_count());
    for repo in state.ctx.repositories.repositories() {
        let repo_name = repo.repository();
        if repo.private {
            let Some(authenticated_client) = authenticated_client.as_ref() else {
                continue;
            };
            if !repo_name
                .is_visible_to_client(authenticated_client)
                .await
                .unwrap_or(false)
            {
                continue;
            }
        }

        let treeclosed = state
            .ctx
            .db
            .repo_db(repo_name)
            .await
            .ok()
            .flatten()
            .is_some_and(|repo| repo.tree_state.is_closed());
        repos.push(RepositoryView {
            name: repo_name.to_string(),
            treeclosed,
        });
    }

    let help_md = format_help();
    let markdown = Parser::new(help_md);

    let mut help_html = String::new();
    pulldown_cmark::html::push_html(&mut help_html, markdown);

    HtmlTemplate(HelpTemplate {
        login_url: oauth
            .as_ref()
            .map(|oauth_client| oauth_client.authorization_url(None)),
        github_user: github_session.as_ref().map(Into::into),
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
    session: SessionNullSession,
    Path((repo_owner, repo_name)): Path<(String, String)>,
    State(db): State<Arc<PgDbClient>>,
    State(oauth): State<Option<OAuthClient>>,
    State(ServerStateRef(state)): State<ServerStateRef>,
    Query(params): Query<QueueParams>,
) -> Result<impl IntoResponse, AppError> {
    let repo_name = GithubRepoName::new(&repo_owner, &repo_name);
    let gh_session = GitHubSession::restore(&session);
    let mut is_visible = false;
    if let Some(gh_session) = gh_session.as_ref() {
        let oauth_client = oauth.as_ref().unwrap();
        let authenticated_client =
            oauth_client.get_authenticated_client(&gh_session.access_token)?;
        is_visible = repo_name
            .is_visible_to_client(&authenticated_client)
            .await?;
    } else if let Some(repo) = state.get_repo(&repo_name) {
        is_visible = !repo.private;
    }

    let repo = match db.repo_db(&repo_name).await? {
        Some(repo) if is_visible => repo,
        _ => {
            return Ok((
                StatusCode::NOT_FOUND,
                format!("Repository {repo_name} not found"),
            )
                .into_response());
        }
    };

    // Perform the queries concurrently to save a bit of time
    let (prs, rollups, last_ten_builds) = futures::future::join3(
        db.get_nonclosed_pull_requests(&repo.name),
        db.get_nonclosed_rollups(&repo.name),
        db.get_last_n_successful_auto_builds(&repo.name, 10),
    )
    .await;
    let prs = prs?;
    let rollups = rollups?;
    let last_ten_builds = last_ten_builds?;

    let prs = sort_queue_prs(prs);

    // Note: this assumed that there is ever at most a single pending build
    let pending_build = prs.iter().find_map(|pr| match pr.queue_status() {
        QueueStatus::Pending(_, build) => Some(build),
        _ => None,
    });
    let pending_workflow = match pending_build {
        Some(build) => db.get_workflows_for_build(build).await?.into_iter().next(),
        None => None,
    };

    let average_build_duration = {
        let total_duration = last_ten_builds
            .iter()
            .filter_map(|build| build.duration)
            .map(|d| d.0)
            .sum::<Duration>();
        let count = last_ten_builds.len() as u32;
        if total_duration.is_zero() {
            // Default guess of 3 hours per build
            Duration::from_secs(3600 * 3)
        } else if count > 1 {
            total_duration / count
        } else {
            total_duration
        }
    };

    let mut in_queue_count = 0;
    let mut failed_count = 0;

    // PR number -> expected remaining duration
    let mut in_queue: HashMap<PullRequestNumber, Duration> = HashMap::new();
    for pr in &prs {
        let status = pr.queue_status();
        let (in_queue_inc, failed_inc) = match &status {
            QueueStatus::Approved(..) => (1, 0),
            QueueStatus::ReadyForMerge(..) => (1, 0),
            QueueStatus::Pending(..) => (1, 0),
            QueueStatus::Failed(..) => (0, 1),
            QueueStatus::NotApproved | QueueStatus::NotOpen => (0, 0),
        };
        in_queue_count += in_queue_inc;
        failed_count += failed_inc;

        match &status {
            QueueStatus::Pending(_, _) => {
                // Try to guess already elapsed time of the pending workflow
                let elapsed = if let Some(workflow) = &pending_workflow {
                    (Utc::now() - workflow.created_at)
                        .to_std()
                        .unwrap_or_default()
                } else {
                    Duration::ZERO
                };
                in_queue.insert(pr.number, average_build_duration.saturating_sub(elapsed));
            }
            // For an approved PR, assume that it will take the average auto build duration
            QueueStatus::Approved(_) => {
                in_queue.insert(pr.number, average_build_duration);
            }
            QueueStatus::Failed(_, _)
            | QueueStatus::ReadyForMerge(_, _)
            | QueueStatus::NotOpen
            | QueueStatus::NotApproved => {}
        }
    }

    let mut expected_remaining_duration: Option<Duration> = None;

    // Rollup members whose rollup is in the queue, and thus its duration will be counted
    let rollup_members: HashSet<PullRequestNumber> = rollups
        .iter()
        .filter(|(rollup, _)| in_queue.contains_key(*rollup))
        .flat_map(|(_, member)| member)
        .copied()
        .collect();

    for (pr, remaining_duration) in in_queue {
        // For a rollup member, we will count its rollup instead
        if rollup_members.contains(&pr) {
            continue;
        }
        expected_remaining_duration =
            Some(expected_remaining_duration.unwrap_or_default() + remaining_duration);
    }

    Ok(HtmlTemplate(QueueTemplate {
        login_url: oauth
            .as_ref()
            .map(|client| client.authorization_url(Some(format!("/queue/{}", repo.name)))),
        github_user: gh_session.as_ref().map(Into::into),
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
        rollups_info: RollupsInfo::from(rollups),
        expected_remaining_duration,
        average_build_duration,
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

/// Query parameters received from GitHub's OAuth callback.
///
/// Documentation: https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#2-users-are-redirected-back-to-your-site-by-github
#[derive(serde::Deserialize)]
struct OAuthCallbackQuery {
    code: OAuthExchangeCode,
    /// Contains the endpoint to redirect to
    state: Option<String>,
}

async fn oauth_callback_handler(
    session: SessionNullSession,
    State(oauth): State<Option<OAuthClient>>,
    Query(query): Query<OAuthCallbackQuery>,
) -> Response {
    tracing::debug!(
        session_id = session.get_session_id(),
        "oauth callback for session"
    );
    let Some(oauth_client) = oauth else {
        let error = anyhow::anyhow!(
            "OAuth not configured. Please set OAUTH_CLIENT_ID and OAUTH_CLIENT_SECRET."
        );
        tracing::error!("{error}");
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("{error:?}")).into_response();
    };

    let prev = GitHubSession::restore(&session);
    match oauth_client.get_access_token(query.code).await {
        Ok(access_token) => {
            if let Err(error) = GitHubSession::save(&session, &oauth_client, access_token).await {
                tracing::warn!("Unable to save GitHub session: {error}");
            }
        }
        Err(error) if prev.is_some() => {
            tracing::warn!("Ignoring invalid GitHub access code: {error}")
        }
        Err(error) => {
            tracing::error!("{error}");
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("{error:?}")).into_response();
        }
    }

    let mut endpoint = None;
    if let Some(query_state) = query.state {
        // don't fail if the endpoint can't be decoded
        match BASE64_URL_SAFE_NO_PAD.decode(&query_state) {
            Ok(bytes) => match std::str::from_utf8(&bytes) {
                Ok(decoded) => endpoint = Some(decoded.to_string()),
                Err(error) => {
                    tracing::warn!(state = query_state, %error, "invalid utf-8 from endpoint")
                }
            },
            Err(error) => tracing::warn!(state = query_state, %error, "invalid base64 encoding"),
        }
    }

    let target = endpoint.as_deref().unwrap_or("/");
    tracing::debug!("Redirecting after oauth to: {target}");
    Redirect::to(target).into_response()
}

async fn logout_handler(session: SessionNullSession) -> Redirect {
    session.destroy();
    Redirect::to("/")
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
            insta::assert_snapshot!(response, @r#"[{"number":1,"title":"Title of PR 1","author":"default-user","status":"open","head_branch":"pr/1","base_branch":"main","priority":null,"approver":"default-user","try_build":null,"auto_build":null}]"#);
            Ok(())
        })
        .await;
    }
}
