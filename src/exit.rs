#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitCode {
    ToolFailed,
    NoServer,
    TunnelFailed,
    NeedsHuman,
    Internal,
}

impl ExitCode {
    pub fn value(self) -> u8 {
        match self {
            Self::ToolFailed => 1,
            Self::NoServer => 2,
            Self::TunnelFailed => 4,
            Self::NeedsHuman => 5,
            Self::Internal => 10,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ExitError {
    pub code: ExitCode,
    pub message: String,
    pub next_action: Option<String>,
}

impl ExitError {
    pub fn new(code: ExitCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), next_action: None }
    }

    #[must_use]
    pub fn next(mut self, action: impl Into<String>) -> Self {
        self.next_action = Some(action.into());
        self
    }
}
