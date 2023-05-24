use anyhow::Error;
use tracing::span::Span;

pub trait LogError {
    fn log_error(&self, error: Error);
}

impl LogError for Span {
    fn log_error(&self, error: Error) {
        self.in_scope(|| {
            tracing::error!("Error: {error:?}");
        });
    }
}
