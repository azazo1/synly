use std::fmt::{Display, Formatter};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    InvalidConfig(&'static str),
    Protocol(String),
    Codec(String),
    Backend(String),
    UnsupportedPlatform(&'static str),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::InvalidConfig(msg) => write!(f, "invalid config: {msg}"),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Self::Codec(msg) => write!(f, "codec error: {msg}"),
            Self::Backend(msg) => write!(f, "backend error: {msg}"),
            Self::UnsupportedPlatform(msg) => write!(f, "unsupported platform: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
