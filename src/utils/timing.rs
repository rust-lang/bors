use crate::github::api::DEFAULT_REQUEST_TIMEOUT;
use itertools::Itertools;
use std::fmt::Debug;
use std::time::{Duration, Instant};
use tokio::time;
use tracing::Instrument;

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

/// Signals if a retryable operation should be retried or not.
pub enum ShouldRetry<E> {
    Yes(E),
    No(E),
}

/// If we have a general error, we convert it to `ShouldRetry::Yes` automatically.
impl From<anyhow::Error> for ShouldRetry<anyhow::Error> {
    fn from(error: anyhow::Error) -> Self {
        Self::Yes(error)
    }
}

/// Decides how will a retryable operation get retried in case it times out or fails.
#[derive(Debug)]
pub struct RetryMethod {
    /// After how much time should the operation time out.
    timeout_after: Duration,
    /// How many total attempts should be performed.
    /// An attempt might be performed either because a timeout happened, or because an error was
    /// returned.
    max_retry_count: u32,
    /// Should we retry when an error was returned from the operation?
    retry_on_error: bool,
    /// For how much time should we sleep in-between attempts.
    backoff_time: Duration,
}

impl RetryMethod {
    pub fn no_retry_on_error() -> Self {
        let mut method = Self::default();
        method.retry_on_error = false;
        method
    }
}

impl Default for RetryMethod {
    fn default() -> Self {
        Self {
            timeout_after: DEFAULT_REQUEST_TIMEOUT,
            max_retry_count: 3,
            retry_on_error: true,
            backoff_time: Duration::from_secs(2),
        }
    }
}

/// Represents the result of a fallible/retryable operation.
#[derive(Debug)]
pub enum RetryableOpError<E> {
    /// The operation ended with an error on its last attempt.
    Err(E),
    /// The operation ended with a timeout on its last attempt.
    /// The data contains all previously encountered errors.
    AllAttemptsExhausted(Vec<anyhow::Error>),
}

impl<E: Into<anyhow::Error>> From<RetryableOpError<E>> for anyhow::Error {
    fn from(value: RetryableOpError<E>) -> Self {
        match value {
            RetryableOpError::Err(error) => error.into(),
            RetryableOpError::AllAttemptsExhausted(errors) => anyhow::anyhow!(
                "All attempts were exhausted, the operation was not performed successfully. Errors:\n{}",
                errors
                    .into_iter()
                    .map(|error| format!("{error:?}"))
                    .join("\n")
            ),
        }
    }
}

/// Perform an asynchronous retryable operation.
///
/// If the operation returns an error and it was retried, the last received error will be returned
/// from the function.
///
/// The caller can explicitly specify which errors should be retried and which shouldn't.
/// By default, all errors will be turned into `ShouldRetry::Yes` due to a blanket impl.
pub async fn perform_retryable<T, E, R, F, Fut>(
    operation_name: &str,
    retry_method: RetryMethod,
    func: F,
) -> Result<T, RetryableOpError<E>>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, R>>,
    R: Into<ShouldRetry<E>>,
    E: Debug,
{
    let span = tracing::trace_span!(
        "Retryable operation",
        operation = operation_name,
        ?retry_method,
    );

    let mut errors = vec![];
    for attempt in 1..=retry_method.max_retry_count {
        let last_attempt = attempt == retry_method.max_retry_count;

        let start = Instant::now();

        span.in_scope(|| {
            tracing::trace!("Attempt number #{attempt}");
        });

        let future =
            tokio::time::timeout(retry_method.timeout_after, func()).instrument(span.clone());
        let duration = start.elapsed();

        let result: Option<Result<T, ShouldRetry<E>>> = match future.await {
            Ok(res) => Some(res.map_err(|e| e.into())),
            Err(_) => None,
        };
        let is_err = match &result {
            Some(Ok(_)) => false,
            Some(Err(_)) => true,
            None => true,
        };

        span.in_scope(|| {
            tracing::trace!(
                attempt = attempt,
                duration_ms = format!("{:.2}", duration.as_secs_f64() * 1000.0),
                "Operation completed {}successfully",
                if is_err { "un" } else { "" }
            );
        });

        match result {
            Some(Ok(res)) => {
                return Ok(res);
            }
            Some(Err(ShouldRetry::Yes(error))) => {
                tracing::error!("Operation failed with error: {error:?}");
                if last_attempt || !retry_method.retry_on_error {
                    return Err(RetryableOpError::Err(error));
                }

                errors.push(anyhow::anyhow!("{error:?}"));
            }
            Some(Err(ShouldRetry::No(error))) => return Err(RetryableOpError::Err(error)),
            None => {
                tracing::error!("Operation timeouted");
                errors.push(anyhow::anyhow!(
                    "Timeout after {}s",
                    retry_method.timeout_after.as_secs_f64()
                ));
            }
        };

        if !last_attempt {
            time::sleep(retry_method.backoff_time).await;
        }
    }

    span.in_scope(|| {
        tracing::trace!(
            "Operation failed after {} retries",
            retry_method.max_retry_count
        );
    });
    Err(RetryableOpError::AllAttemptsExhausted(errors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Duration;
    use tokio::time::sleep;

    #[derive(Default)]
    struct RetryCounter(Cell<u32>);

    impl RetryCounter {
        fn increment(&self) {
            self.0.set(self.0.get() + 1);
        }

        fn get(&self) -> u32 {
            self.0.get()
        }
    }

    #[tokio::test]
    async fn test_retry_timeout() {
        let counter = RetryCounter::default();

        perform_retryable(
            "test_op",
            RetryMethod {
                timeout_after: Duration::from_millis(10),
                max_retry_count: 3,
                retry_on_error: false,
                backoff_time: Duration::from_millis(1),
            },
            || async {
                counter.increment();
                sleep(Duration::from_millis(50)).await;
                anyhow::Ok(42)
            },
        )
        .await
        .expect_err("no error found");
        assert_eq!(counter.get(), 3);
    }

    #[tokio::test]
    async fn test_retry_error() {
        let counter = RetryCounter::default();

        let error: anyhow::Error = perform_retryable(
            "test_op",
            RetryMethod {
                timeout_after: Duration::from_secs(1000),
                max_retry_count: 4,
                retry_on_error: true,
                backoff_time: Duration::from_millis(1),
            },
            || async {
                counter.increment();
                Err::<(), _>(anyhow::anyhow!("FooBarFail"))
            },
        )
        .await
        .map_err(|e| e.into())
        .expect_err("no error found");

        assert!(format!("{error:?}").contains("FooBarFail"));
        assert_eq!(counter.get(), 4);
    }

    #[tokio::test]
    async fn test_retry_ok() {
        let counter = RetryCounter::default();

        let result = perform_retryable(
            "test_op",
            RetryMethod {
                timeout_after: Duration::from_secs(1000),
                max_retry_count: 4,
                retry_on_error: false,
                backoff_time: Duration::from_millis(1),
            },
            || async {
                counter.increment();
                anyhow::Ok(42)
            },
        )
        .await
        .unwrap();

        assert_eq!(result, 42);
        assert_eq!(counter.get(), 1);
    }

    #[tokio::test]
    async fn test_manual_retry_no() {
        let counter = RetryCounter::default();

        let error: RetryableOpError<anyhow::Error> = perform_retryable(
            "test_op",
            RetryMethod {
                timeout_after: Duration::from_secs(1000),
                max_retry_count: 5,
                retry_on_error: true,
                backoff_time: Duration::from_millis(1),
            },
            || async {
                counter.increment();
                Err::<(), _>(ShouldRetry::No(anyhow::anyhow!("FooBarFail")))
            },
        )
        .await
        .expect_err("no error found");

        assert!(format!("{error:?}").contains("FooBarFail"));
        assert_eq!(counter.get(), 1);
    }

    #[tokio::test]
    async fn test_manual_retry_yes() {
        let counter = RetryCounter::default();

        let result = perform_retryable::<i32, anyhow::Error, _, _, _>(
            "test_op",
            RetryMethod {
                timeout_after: Duration::from_secs(1000),
                max_retry_count: 5,
                retry_on_error: true,
                backoff_time: Duration::from_millis(1),
            },
            || async {
                counter.increment();
                if counter.get() == 3 {
                    Ok(42)
                } else {
                    Err(ShouldRetry::Yes(anyhow::anyhow!("FooBarFail")))
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(result, 42);
        assert_eq!(counter.get(), 3);
    }
}
