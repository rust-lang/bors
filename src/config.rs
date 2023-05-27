use serde::Deserialize;
use std::time::Duration;

pub const CONFIG_FILE_PATH: &str = "rust-bors.toml";

/// Configuration of a repository loaded from a `rust-bors.toml`
/// file located in the root of the repository file tree.
#[derive(serde::Deserialize, Debug)]
pub struct RepositoryConfig {
    #[serde(default = "default_timeout", deserialize_with = "duration_from_secs")]
    pub timeout: Duration,
}

fn default_timeout() -> Duration {
    Duration::from_secs(3600)
}

fn duration_from_secs<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let seconds = u64::deserialize(deserializer)?;
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use crate::config::RepositoryConfig;

    #[test]
    fn deserialize_timeout() {
        let content = "timeout = 3600";
        let config = load_config(content);
        assert_eq!(config.timeout.as_secs(), 3600);
    }

    fn load_config(config: &str) -> RepositoryConfig {
        toml::from_str(config).unwrap()
    }
}
