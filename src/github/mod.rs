pub mod webhook;

/// Unique identifier of a GitHub repository
#[derive(Debug)]
pub struct GitHubRepositoryKey {
    owner: String,
    name: String,
}

impl GitHubRepositoryKey {
    pub fn new(owner: &str, name: &str) -> Self {
        Self {
            owner: owner.to_lowercase(),
            name: name.to_lowercase(),
        }
    }
}
