pub mod parser;

/// Bors command specified by a user.
#[derive(Debug)]
pub enum BorsCommand {
    Ping,
    Try,
}
