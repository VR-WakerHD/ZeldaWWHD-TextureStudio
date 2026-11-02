use std::fmt;

/// Error for every fallible operation in the crate. Format errors carry a
/// human-readable reason; nothing is silently skipped.
#[derive(Debug)]
pub enum Error {
    Format(String),
    Unsupported(String),
    Io(std::io::Error),
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Format(s) => write!(f, "malformed data: {s}"),
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Format(format!("json: {e}"))
    }
}

macro_rules! bail {
    ($($arg:tt)*) => { return Err($crate::error::Error::Format(format!($($arg)*))) };
}
macro_rules! unsupported {
    ($($arg:tt)*) => { return Err($crate::error::Error::Unsupported(format!($($arg)*))) };
}
pub(crate) use bail;
pub(crate) use unsupported;
