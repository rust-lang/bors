mod parser;
pub use parser::{CommandParseError, CommandParser};

/// Bors command specified by a user.
#[derive(Debug)]
pub enum BorsCommand {
    /// Ping the bot.
    Ping,
    /// Perform a try build.
    Try,
    /// Cancel a try build.
    TryCancel,
}
