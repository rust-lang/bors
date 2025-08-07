use std::time::{Duration, Instant};
use tokio::time;

use crate::github::api::{DEFAULT_REQUEST_TIMEOUT, DEFAULT_RETRY_COUNT};

const DEFAULT_BACKOFF_TIME: Duration = Duration::from_secs(1);

/// Measure the duration of an async operation and logs it using tracing.
pub async fn measure_operation<T, F, Fut>(operation_name: &str, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    let start = Instant::now();

    tracing::trace!(operation = operation_name, "Starting operation");

    let result = f().await;
    let duration = start.elapsed();

    tracing::trace!(
        operation = operation_name,
        duration_ms = format!("{:.2}", duration.as_secs_f64() * 1000.0),
        "Operation completed"
    );

    result
}

/// Measures the duration of a database query and logs it using tracing.
pub async fn measure_db_query<T, F, Fut>(query_name: &str, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    measure_operation(&format!("db_query:{query_name}"), f).await
}

/// Performs an async operation while checking for a timeout.
/// If a timeout happens, retries the operation several times.
///
/// Returns an `Err` if the operation timed out several times in a row.
pub async fn retry_on_timeout<T, F, Fut>(operation_name: &str, f: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = T>,
{
    tracing::trace!(operation = operation_name, "Starting operation");

    for attempt in 0..=DEFAULT_RETRY_COUNT {
        let start = Instant::now();

        match time::timeout(DEFAULT_REQUEST_TIMEOUT, f()).await {
            Ok(result) => {
                let duration = start.elapsed();
                tracing::trace!(
                    operation = operation_name,
                    attempt = attempt,
                    duration_ms = format!("{:.2}", duration.as_secs_f64() * 1000.0),
                    "Operation completed successfully"
                );
                return Ok(result);
            }
            Err(_) => {
                tracing::debug!(
                    operation = operation_name,
                    attempt = attempt,
                    "Operation timed out"
                );
                if attempt < DEFAULT_RETRY_COUNT {
                    tracing::trace!(
                        operation = operation_name,
                        attempt = attempt + 1,
                        "Retrying operation..."
                    );
                }
            }
        }

        time::sleep(DEFAULT_BACKOFF_TIME).await;
    }

    tracing::trace!(
        operation = operation_name,
        retries = DEFAULT_RETRY_COUNT,
        "Operation failed after all retries"
    );
    Err(anyhow::anyhow!(
        "Operation '{operation_name}' timed out after {DEFAULT_RETRY_COUNT} retries",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_measure_operation() {
        // Test basic timing functionality
        async fn sample_operation() -> i32 {
            sleep(Duration::from_millis(50)).await;
            42
        }

        let result = retry_on_timeout("test_op", sample_operation).await;
        assert_eq!(result.unwrap_or_default(), 42);
    }

    #[tokio::test]
    async fn test_error_handling() {
        // Test that errors are properly propagated
        async fn failing_operation() -> Result<i32, String> {
            sleep(Duration::from_millis(50)).await;
            Err("test error".to_string())
        }

        let result = retry_on_timeout("error_test", failing_operation)
            .await
            .unwrap_or(Ok(0));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "test error");
    }
}
