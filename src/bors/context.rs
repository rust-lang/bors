use crate::bors::command::CommandParser;

pub struct BorsContext {
    pub parser: CommandParser,
}

impl BorsContext {
    pub fn new(parser: CommandParser) -> Self {
        Self { parser }
    }
}
