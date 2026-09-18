use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("provider error: {0}")]
    Provider(String),

    #[error("tool error: {0}")]
    Tool(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("context window exceeded (used ~{used}, limit {limit})")]
    ContextWindowExceeded { used: i64, limit: i64 },

    #[error("channel closed: {0}")]
    ChannelClosed(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    /// The API rejected the request because the conversation no longer fits
    /// the context window. Providers map their own error shapes onto this.
    pub fn is_context_window_exceeded(&self) -> bool {
        matches!(self, Error::ContextWindowExceeded { .. })
    }
}
