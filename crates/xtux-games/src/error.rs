use std::fmt::Display;

/// Errors of game level operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// The account is not entitled to the content.
    #[error("not entitled to this content: {0}")]
    NotEntitled(String),
    /// No user or device is signed in, or its tokens are unusable.
    #[error("not signed in: {0}")]
    SignedOut(String),
    /// The operation was cancelled through its cancellation token.
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Message(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn msg(text: impl Display) -> Self {
        Error::Message(text.to_string())
    }
}

impl From<String> for Error {
    fn from(text: String) -> Self {
        Error::Message(text)
    }
}

impl From<&str> for Error {
    fn from(text: &str) -> Self {
        Error::Message(text.to_string())
    }
}
