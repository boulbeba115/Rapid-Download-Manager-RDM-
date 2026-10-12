use std::io;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("network error: {0}")]
    Network(String),
    #[error("server returned HTTP {0}")]
    Status(u16),
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("{0}")]
    Other(String),
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Network(describe(&e))
    }
}

/// reqwest's Display hides the root cause ("error sending request"); walk the
/// source chain so the user sees "connection refused" / "dns error" etc.
pub fn describe(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        let part = s.to_string();
        if !msg.contains(&part) {
            msg.push_str(": ");
            msg.push_str(&part);
        }
        src = s.source();
    }
    msg
}
