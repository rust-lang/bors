use anyhow::anyhow;
use std::time::{Duration, Instant};
use tokio::time;
use tracing::trace;

// Measures the duration of an async operation and logs it using tracing.
pub async fn measure_operation<T, F, Fut>(operation_name: &str, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let start = Instant::now();

    trace!(operation = operation_name, "Starting operation");

    let result = f().await;
    let duration = start.elapsed();

    trace!(
        operation = operation_name,
        duration_ms = format!("{:.2}", duration.as_secs_f64() * 1000.0),
        "Operation completed"
    );

    result
}

// Measures the duration of an async operation and retries it if it fails.
pub async fn measure_operation_with_retry<T, F, Fut>(
    operation_name: &str,
    f: &F,
    timeout: Duration,
    num_retries: u32,
) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    trace!(operation = operation_name, "Starting operation");

    for attempt in 0..=num_retries {
        let start = Instant::now();

        match time::timeout(timeout, f()).await {
            Ok(result) => {
                let duration = start.elapsed();
                trace!(
                    operation = operation_name,
                    attempt = attempt,
                    duration_ms = format!("{:.2}", duration.as_secs_f64() * 1000.0),
                    "Operation completed successfully"
                );
                return Ok(result);
            }
            Err(_) => {
                trace!(
                    operation = operation_name,
                    attempt = attempt,
                    "Operation timed out"
                );
                if attempt < num_retries {
                    trace!(
                        operation = operation_name,
                        attempt = attempt + 1,
                        "Retrying operation..."
                    );
                }
            }
        }
    }

    trace!(
        operation = operation_name,
        retries = num_retries,
        "Operation failed after all retries"
    );
    Err(anyhow!(
        "Operation '{}' timed out after {} retries",
        operation_name,
        num_retries
    ))
}

// Measures the duration of a database query and logs it using tracing.
pub async fn measure_db_query<T, F, Fut>(query_name: &str, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    measure_operation(&format!("db_query:{query_name}"), f).await
}

// Measures the duration of a network request and logs it using tracing.
pub async fn perform_network_request<T, F, Fut>(
    request_name: &str,
    f: F,
    timeout: Duration,
    num_retry: u32,
) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    measure_operation_with_retry(
        &format!("network_request:{request_name}"),
        &f,
        timeout,
        num_retry,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::sleep;

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_measure_operation() {
        // Test basic timing functionality
        async fn sample_operation() -> i32 {
            sleep(Duration::from_millis(50)).await;
            42
        }

        let result =
            measure_operation_with_retry("test_op", &sample_operation, Duration::from_secs(1), 1)
                .await;
        assert_eq!(result.unwrap_or_default(), 42);
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_error_handling() {
        // Test that errors are properly propagated
        async fn failing_operation() -> Result<i32, String> {
            sleep(Duration::from_millis(50)).await;
            Err("test error".to_string())
        }

        let result = measure_operation_with_retry(
            "error_test",
            &failing_operation,
            Duration::from_secs(1),
            1,
        )
        .await
        .unwrap_or(Ok(0));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "test error");
    }
}
