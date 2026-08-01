use chrono::{DateTime, Utc};
use std::sync::RwLock;
use std::time::Duration;

/// Cached value with some maximum staleness
pub struct Cached<T> {
    value: RwLock<Option<CachedValue<T>>>,
    max_stale_duration: chrono::Duration,
}

impl<T: Clone> Cached<T> {
    pub fn new(max_stale_duration: Duration) -> Self {
        Self {
            value: RwLock::new(None),
            max_stale_duration: chrono::Duration::from_std(max_stale_duration).unwrap(),
        }
    }

    pub async fn load<F>(&self, func: F) -> anyhow::Result<CachedValue<T>>
    where
        F: AsyncFnOnce() -> anyhow::Result<T>,
    {
        {
            let lock = self.value.read().unwrap();
            if let Some(value) = lock.as_ref()
                && Utc::now().signed_duration_since(value.loaded_at) < self.max_stale_duration
            {
                // Cache hit
                return Ok(value.clone());
            }
        }

        // Try to load a new value
        let value = func().await?;

        let cached_value = CachedValue {
            value,
            loaded_at: Utc::now(),
        };
        let mut lock = self.value.write().unwrap();
        *lock = Some(cached_value.clone());

        Ok(cached_value)
    }
}

#[derive(Clone)]
pub struct CachedValue<T> {
    pub value: T,
    pub loaded_at: DateTime<Utc>,
}
