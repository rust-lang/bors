use std::collections::HashMap;
use std::time::Duration;

use serde::de::Error;
use serde::{Deserialize, Deserializer};

use crate::github::{LabelModification, LabelTrigger};

pub const CONFIG_FILE_PATH: &str = "rust-bors.toml";

/// Configuration of a repository loaded from a `rust-bors.toml`
/// file located in the root of the repository file tree.
#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    /// Maximum duration (in seconds) to wait for CI checks to complete before timing out.
    /// Defaults to 3600 seconds (1 hour).
    #[serde(
        default = "default_timeout",
        deserialize_with = "deserialize_duration_from_secs"
    )]
    pub timeout: Duration,
    /// Label modifications to apply when specific events occur.
    /// Maps trigger events (approve, try, etc.) to label additions/removals.
    /// Format: `trigger = ["+label_to_add", "-label_to_remove"]`
    #[serde(default, deserialize_with = "deserialize_labels")]
    pub labels: HashMap<LabelTrigger, Vec<LabelModification>>,
    /// Labels that will block a PR from being approved when present on the PR.
    #[serde(default)]
    pub labels_blocking_approval: Vec<String>,
    /// Minimum time (in seconds) to wait for CI checks to complete before proceeding.
    /// Defaults to `None` (no minimum wait time).
    #[serde(default, deserialize_with = "deserialize_duration_from_secs_opt")]
    pub min_ci_time: Option<Duration>,
    /// Whether auto merging is enabled.
    /// Defaults to false.
    #[serde(default)]
    pub merge_queue_enabled: bool,
}

/// Load a repository config from TOML.
pub fn deserialize_config(text: &str) -> Result<RepositoryConfig, toml::de::Error> {
    toml::from_str(text)
}

fn default_timeout() -> Duration {
    Duration::from_secs(3600)
}

fn deserialize_duration_from_secs_opt<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: Deserializer<'de>,
{
    // Allow null values for the option
    let maybe_seconds = Option::<u64>::deserialize(deserializer)?;
    Ok(maybe_seconds.map(Duration::from_secs))
}

fn deserialize_duration_from_secs<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let seconds = u64::deserialize(deserializer)?;
    Ok(Duration::from_secs(seconds))
}

fn deserialize_labels<'de, D>(
    deserializer: D,
) -> Result<HashMap<LabelTrigger, Vec<LabelModification>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(serde::Deserialize, Eq, PartialEq, Hash)]
    #[serde(rename_all = "snake_case")]
    enum Trigger {
        Approved,
        Unapproved,
        TryFailed,
        AutoBuildSucceeded,
        AutoBuildFailed,
    }

    impl From<Trigger> for LabelTrigger {
        fn from(value: Trigger) -> Self {
            match value {
                Trigger::Approved => LabelTrigger::Approved,
                Trigger::Unapproved => LabelTrigger::Unapproved,
                Trigger::TryFailed => LabelTrigger::TryBuildFailed,
                Trigger::AutoBuildSucceeded => LabelTrigger::AutoBuildSucceeded,
                Trigger::AutoBuildFailed => LabelTrigger::AutoBuildFailed,
            }
        }
    }

    enum Modification {
        Add(String),
        Remove(String),
    }

    impl From<Modification> for LabelModification {
        fn from(value: Modification) -> Self {
            match value {
                Modification::Add(label) => LabelModification::Add(label),
                Modification::Remove(label) => LabelModification::Remove(label),
            }
        }
    }

    impl<'de> serde::Deserialize<'de> for Modification {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            let value: String = String::deserialize(deserializer)?;
            if value.len() < 2 {
                return Err(Error::custom(
                    "Label modification have at least two characters and start with `+` or `-`",
                ));
            }

            let modification = if let Some(label) = value.strip_prefix('+') {
                Modification::Add(label.to_string())
            } else if let Some(label) = value.strip_prefix('-') {
                Modification::Remove(label.to_string())
            } else {
                return Err(Error::custom(
                    "Label modification must start with `+` or `-`",
                ));
            };

            Ok(modification)
        }
    }

    let mut triggers = HashMap::<Trigger, Vec<Modification>>::deserialize(deserializer)?;
    // If there are any `approve` triggers, add `unapprove` triggers as well.
    if let Some(modifications) = triggers.get(&Trigger::Approved) {
        let unapprove_modifications = modifications
            .iter()
            .map(|m| match m {
                Modification::Add(label) => Modification::Remove(label.clone()),
                Modification::Remove(label) => Modification::Add(label.clone()),
            })
            .collect::<Vec<_>>();
        triggers
            .entry(Trigger::Unapproved)
            .or_insert_with(|| unapprove_modifications);
    }
    let triggers = triggers
        .into_iter()
        .map(|(k, v)| (k.into(), v.into_iter().map(|v| v.into()).collect()))
        .collect();
    Ok(triggers)
}

#[cfg(test)]
mod tests {
    use crate::config::{RepositoryConfig, default_timeout, deserialize_config};
    use std::{collections::BTreeMap, time::Duration};

    #[test]
    fn deserialize_empty() {
        let content = "";
        let config = load_config(content);
        assert_eq!(config.timeout, default_timeout());
    }

    #[test]
    fn deserialize_timeout() {
        let content = "timeout = 3600";
        let config = load_config(content);
        assert_eq!(config.timeout.as_secs(), 3600);
    }

    #[test]
    fn deserialize_min_ci_time_empty() {
        let content = "";
        let config = load_config(content);
        assert_eq!(config.min_ci_time, None);
    }

    #[test]
    fn deserialize_min_ci_time() {
        let content = "min_ci_time = 3600";
        let config = load_config(content);
        assert_eq!(config.min_ci_time, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn deserialize_merge_queue_enabled_default() {
        let content = "";
        let config = load_config(content);
        assert!(!config.merge_queue_enabled);
    }

    #[test]
    fn deserialize_merge_queue_enabled_true() {
        let content = "merge_queue_enabled = true";
        let config = load_config(content);
        assert!(config.merge_queue_enabled);
    }

    #[test]
    fn deserialize_merge_queue_enabled_false() {
        let content = "merge_queue_enabled = false";
        let config = load_config(content);
        assert!(!config.merge_queue_enabled);
    }

    #[test]
    fn deserialize_labels() {
        let content = r#"[labels]
approved = ["+approved"]
unapproved = ["-approved"]
try_failed = []
auto_build_succeeded = ["+foobar", "-foo"]
auto_build_failed = ["+bar", "+baz"]
"#;
        let config = load_config(content);
        insta::assert_debug_snapshot!(config.labels.into_iter().collect::<BTreeMap<_, _>>(), @r#"
        {
            Approved: [
                Add(
                    "approved",
                ),
            ],
            Unapproved: [
                Remove(
                    "approved",
                ),
            ],
            TryBuildFailed: [],
            AutoBuildSucceeded: [
                Add(
                    "foobar",
                ),
                Remove(
                    "foo",
                ),
            ],
            AutoBuildFailed: [
                Add(
                    "bar",
                ),
                Add(
                    "baz",
                ),
            ],
        }
        "#);
    }

    #[test]
    #[should_panic(expected = "Label modification must start with `+` or `-`")]
    fn deserialize_labels_missing_prefix() {
        let content = r#"[labels]
approved = ["foo"]
"#;
        load_config(content);
    }

    #[test]
    fn deserialize_labels_blocking_approval() {
        let content = r#"labels_blocking_approval = ["foo", "bar"]"#;
        let config = load_config(content);
        insta::assert_debug_snapshot!(config.labels_blocking_approval, @r#"
        [
            "foo",
            "bar",
        ]
        "#);
    }

    #[test]
    #[should_panic(expected = "unknown field `labels-blocking-approval`")]
    fn deserialize_unknown_key_fail() {
        let content = r#"labels-blocking-approval = ["foo", "bar"]"#;
        load_config(content);
    }

    fn load_config(config: &str) -> RepositoryConfig {
        deserialize_config(config).expect("Cannot deserialize repository config")
    }
}
