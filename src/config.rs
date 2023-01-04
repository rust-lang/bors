use secrecy::SecretString;

/// Configuration of `bors` used to communicate with GitHub.
#[derive(Clone)]
pub struct Config {
    pub webhook_secret: SecretString,
}

impl Config {
    pub fn new(webhook_secret: String) -> Self {
        Self {
            webhook_secret: webhook_secret.into(),
        }
    }
}
