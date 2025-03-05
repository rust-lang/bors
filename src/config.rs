use std::collections::HashMap;
use std::time::Duration;

use serde::de::Error;
use serde::{Deserialize, Deserializer};

use crate::github::{LabelModification, LabelTrigger};

pub const CONFIG_FILE_PATH: &str = "rust-bors.toml";

/// Configuration of a repository loaded from a `rust-bors.toml`
/// file located in the root of the repository file tree.
#[derive(serde::Deserialize, Debug)]
pub struct RepositoryConfig {
    #[serde(
        default = "default_timeout",
        deserialize_with = "deserialize_duration_from_secs"
    )]
    pub timeout: Duration,
    #[serde(default, deserialize_with = "deserialize_labels")]
    pub labels: HashMap<LabelTrigger, Vec<LabelModification>>,
    #[serde(default, deserialize_with = "deserialize_duration_from_secs_opt")]
    pub min_ci_time: Option<Duration>,
    #[serde(default)]
    pub treeclosed: Option<u32>,
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
        Approve,
        Unapprove,
        Try,
        TrySucceed,
        TryFailed,
    }

    impl From<Trigger> for LabelTrigger {
        fn from(value: Trigger) -> Self {
            match value {
                Trigger::Approve => LabelTrigger::Approved,
                Trigger::Unapprove => LabelTrigger::Unapproved,
                Trigger::Try => LabelTrigger::TryBuildStarted,
                Trigger::TrySucceed => LabelTrigger::TryBuildSucceeded,
                Trigger::TryFailed => LabelTrigger::TryBuildFailed,
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
    if let Some(modifications) = triggers.get(&Trigger::Approve) {
        let unapprove_modifications = modifications
            .iter()
            .map(|m| match m {
                Modification::Add(label) => Modification::Remove(label.clone()),
                Modification::Remove(label) => Modification::Add(label.clone()),
            })
            .collect::<Vec<_>>();
        triggers
            .entry(Trigger::Unapprove)
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
    use std::{collections::BTreeMap, time::Duration};

    use crate::config::{default_timeout, RepositoryConfig};

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
    fn deserialize_labels() {
        let content = r#"[labels]
approve = ["+approved"]
try = ["+foo", "-bar"]
try_succeed = ["+foobar", "+foo", "+baz"]
try_failed = []
"#;
        let config = load_config(content);
        insta::assert_debug_snapshot!(config.labels.into_iter().collect::<BTreeMap<_, _>>(), @r###"
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
            TryBuildStarted: [
                Add(
                    "foo",
                ),
                Remove(
                    "bar",
                ),
            ],
            TryBuildSucceeded: [
                Add(
                    "foobar",
                ),
                Add(
                    "foo",
                ),
                Add(
                    "baz",
                ),
            ],
            TryBuildFailed: [],
        }
        "###);
    }

    #[test]
    #[should_panic(expected = "Label modification must start with `+` or `-`")]
    fn deserialize_labels_missing_prefix() {
        let content = r#"[labels]
try = ["foo"]
"#;
        load_config(content);
    }

    #[test]
    fn deserialize_treeclosed() {
        let content = "treeclosed = 5";
        let config = load_config(content);
        assert_eq!(config.treeclosed, Some(5));
    }

    #[test]
    fn deserialize_treeclosed_empty() {
        let content = "";
        let config = load_config(content);
        assert_eq!(config.treeclosed, None);
    }

    fn load_config(config: &str) -> RepositoryConfig {
        toml::from_str(config).unwrap()
    }
}
