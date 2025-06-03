use anyhow::Error as AnyhowError;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub struct AppError(pub AnyhowError);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let msg = format!("Something went wrong: {}", self.0);
        tracing::error!("{msg}");
        (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<AnyhowError>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}
