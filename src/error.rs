use std::net::SocketAddr;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    // The cause is the error's `source()`: repeating it in the message would
    // print it twice in any chain-aware reporter (anyhow, tracing).
    #[error("failed to read config file")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config file")]
    Parse(#[from] toml::de::Error),
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

#[derive(Debug, Error)]
pub enum ExifError {
    #[error("failed to read file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse metadata: {0}")]
    Parse(String),
    #[error("no capture date found in metadata")]
    NoDate,
}

#[derive(Debug, Error)]
pub enum NamingError {
    #[error("invalid pattern token: {0}")]
    InvalidToken(String),
    #[error("invalid pattern: {0}")]
    InvalidPattern(String),
}

#[derive(Debug, Error)]
pub enum DedupError {
    #[error("failed to hash {path}: {source}")]
    Hash {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("state file error: {0}")]
    State(String),
}

#[derive(Debug, Error)]
pub enum StagingError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("journal error: {0}")]
    Journal(String),
}

#[derive(Debug, Error)]
pub enum ScanError {
    #[error("failed to walk {path}: {message}")]
    Walk { path: PathBuf, message: String },
    #[error(transparent)]
    Staging(#[from] StagingError),
    #[error(transparent)]
    Dedup(#[from] DedupError),
}

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("invalid URL {url:?}: {reason}")]
    Url { url: String, reason: String },
    #[error("cannot resolve {0}")]
    Resolve(String),
    #[error("cannot connect to {addr}: {source}")]
    Connect {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error("no response: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed HTTP response")]
    Malformed,
}

#[derive(Debug, Error)]
pub enum NotifyError {
    #[error("cannot write the event file in {dir}: {source}")]
    File {
        dir: PathBuf,
        source: std::io::Error,
    },
    #[error("webhook {url}: {source}")]
    Webhook { url: String, source: HttpError },
    #[error("webhook {url} answered HTTP {code}")]
    WebhookStatus { url: String, code: u16 },
}

#[derive(Debug, Error)]
pub enum HealthError {
    #[error("failed to bind health endpoint on {addr}: {message}")]
    Bind { addr: String, message: String },
    #[error("health probe failed: {0}")]
    Probe(String),
}
