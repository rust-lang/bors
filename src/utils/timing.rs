use std::time::Instant;
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

// Measures the duration of a database query and logs it using tracing.
pub async fn measure_db_query<T, F, Fut>(query_name: &str, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    measure_operation(&format!("db_query:{query_name}"), f).await
}

// Measures the duration of a network request and logs it using tracing.
pub async fn measure_network_request<T, F, Fut>(request_name: &str, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    measure_operation(&format!("network_request:{request_name}"), f).await
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

        let result = measure_operation("test_op", || sample_operation()).await;
        assert_eq!(result, 42);
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_error_handling() {
        // Test that errors are properly propagated
        async fn failing_operation() -> Result<i32, String> {
            sleep(Duration::from_millis(50)).await;
            Err("test error".to_string())
        }

        let result = measure_operation("error_test", || failing_operation()).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "test error");
    }
}
