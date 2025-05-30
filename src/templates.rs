use askama::Template;
use axum::response::{Html, IntoResponse, Response};
use http::StatusCode;

pub struct HtmlTemplate<T>(pub T);

impl<T> IntoResponse for HtmlTemplate<T>
where
    T: Template,
{
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(html) => Html(html).into_response(),
            Err(error) => {
                let message = format!("Failed to render template: {error:?}");
                tracing::error!("{message}");
                (StatusCode::INTERNAL_SERVER_ERROR, message).into_response()
            }
        }
    }
}

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    pub repos: Vec<RepositoryView>,
}

pub struct RepositoryView {
    pub name: String,
    pub treeclosed: bool,
}

#[derive(Template)]
#[template(path = "not_found.html")]
pub struct NotFoundTemplate {}
