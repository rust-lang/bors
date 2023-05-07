pub const CONFIG_FILE_PATH: &str = "rust-bors.toml";

/// Configuration of a repository loaded from a `rust-bors.toml`
/// file located in the root of the repository file tree.
#[derive(serde::Deserialize, Debug)]
pub struct RepositoryConfig {}
