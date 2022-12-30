pub mod parser;

/// Command specified by a user in a comment.
pub enum BorsCommand {
    Ping,
    Try,
}
