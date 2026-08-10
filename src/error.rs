//! Unified error types with stable error codes and recovery classification.
//!
//! Codes E001-E005 follow the recovery strategy table in SPECS.txt
//! (Appendix C). Additional codes cover the remaining failure classes.

use std::fmt;
use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Stable, machine-readable error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// E001: The requested file does not exist.
    FileNotFound,
    /// E002: A sandboxed command exceeded its time limit.
    CommandTimeout,
    /// E003: The LLM gateway failed.
    Llm,
    /// E004: A Rhai script failed to compile or run.
    Script,
    /// E005: The task budget is exhausted.
    BudgetExhausted,
    /// E006: The caller lacks the required capability.
    CapabilityDenied,
    /// E007: A path escaped the sandbox root.
    PathViolation,
    /// E008: A filesystem operation failed.
    Io,
    /// E009: The event store (SQLite) failed.
    Database,
    /// E010: Configuration is invalid.
    InvalidConfig,
    /// E011: External input is invalid or malformed.
    InvalidInput,
    /// E012: An asynchronous operation exceeded its time limit.
    Timeout,
    /// E013: A retryable external request was exhausted after retries.
    RetryExhausted,
    /// E014: A network request failed.
    Http,
    /// E015: JSON serialization or deserialization failed.
    Json,
    /// E016: An atomic file write failed.
    AtomicWrite,
    /// E017: Evolution generated a rejected or invalid script.
    EvolutionRejected,
    /// E018: An unrecoverable internal invariant was violated.
    Internal,
}

impl ErrorCode {
    /// The stable code string used in reports and logs, such as "E001".
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::FileNotFound => "E001",
            ErrorCode::CommandTimeout => "E002",
            ErrorCode::Llm => "E003",
            ErrorCode::Script => "E004",
            ErrorCode::BudgetExhausted => "E005",
            ErrorCode::CapabilityDenied => "E006",
            ErrorCode::PathViolation => "E007",
            ErrorCode::Io => "E008",
            ErrorCode::Database => "E009",
            ErrorCode::InvalidConfig => "E010",
            ErrorCode::InvalidInput => "E011",
            ErrorCode::Timeout => "E012",
            ErrorCode::RetryExhausted => "E013",
            ErrorCode::Http => "E014",
            ErrorCode::Json => "E015",
            ErrorCode::AtomicWrite => "E016",
            ErrorCode::EvolutionRejected => "E017",
            ErrorCode::Internal => "E018",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The unified error type for the Lemon Agent core.
#[derive(Debug, Error)]
pub enum Error {
    #[error("E001 file not found: {0}")]
    FileNotFound(PathBuf),

    #[error("E002 command timed out after {timeout_secs}s: {command}")]
    CommandTimeout { command: String, timeout_secs: u64 },

    #[error("E003 LLM error: {message}")]
    Llm { message: String, retryable: bool },

    #[error("E004 script error in {script}: {message}")]
    Script { script: String, message: String },

    #[error("E005 budget exhausted: {0}")]
    BudgetExhausted(String),

    #[error("E006 capability denied: {operation} ({reason})")]
    CapabilityDenied { operation: String, reason: String },

    #[error("E007 path violation: {path:?} escapes sandbox root {root:?}")]
    PathViolation { path: PathBuf, root: PathBuf },

    #[error("E008 I/O error{path}: {source}", path = .path.as_ref().map(|p| format!(" on {p:?}")).unwrap_or_default())]
    Io {
        path: Option<PathBuf>,
        #[source]
        source: io::Error,
    },

    #[error("E009 database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("E010 invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("E011 invalid input: {0}")]
    InvalidInput(String),

    #[error("E012 operation timed out after {timeout_secs}s: {operation}")]
    Timeout {
        operation: String,
        timeout_secs: u64,
    },

    #[error("E013 retry exhausted after {attempts} attempts: {operation} ({message})")]
    RetryExhausted {
        operation: String,
        attempts: u32,
        message: String,
    },

    #[error("E014 HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("E015 JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("E016 atomic write failed for {path:?}: {source}")]
    AtomicWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("E017 evolution rejected: {0}")]
    EvolutionRejected(String),

    #[error("E018 internal error: {0}")]
    Internal(String),
}

impl Error {
    /// The stable error code for this error.
    pub fn code(&self) -> ErrorCode {
        match self {
            Error::FileNotFound(_) => ErrorCode::FileNotFound,
            Error::CommandTimeout { .. } => ErrorCode::CommandTimeout,
            Error::Llm { .. } => ErrorCode::Llm,
            Error::Script { .. } => ErrorCode::Script,
            Error::BudgetExhausted(_) => ErrorCode::BudgetExhausted,
            Error::CapabilityDenied { .. } => ErrorCode::CapabilityDenied,
            Error::PathViolation { .. } => ErrorCode::PathViolation,
            Error::Io { .. } => ErrorCode::Io,
            Error::Database(_) => ErrorCode::Database,
            Error::InvalidConfig(_) => ErrorCode::InvalidConfig,
            Error::InvalidInput(_) => ErrorCode::InvalidInput,
            Error::Timeout { .. } => ErrorCode::Timeout,
            Error::RetryExhausted { .. } => ErrorCode::RetryExhausted,
            Error::Http(_) => ErrorCode::Http,
            Error::Json(_) => ErrorCode::Json,
            Error::AtomicWrite { .. } => ErrorCode::AtomicWrite,
            Error::EvolutionRejected(_) => ErrorCode::EvolutionRejected,
            Error::Internal(_) => ErrorCode::Internal,
        }
    }

    /// Whether retrying the same operation is likely to succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Llm { retryable, .. } => *retryable,
            Error::Timeout { .. }
            | Error::RetryExhausted { .. }
            | Error::CommandTimeout { .. }
            | Error::Http(_) => true,
            Error::Database(rusqlite::Error::SqliteFailure(e, _)) => {
                e.code == rusqlite::ErrorCode::DatabaseBusy
            }
            _ => false,
        }
    }

    /// Whether the agent can continue after reporting this error.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Error::BudgetExhausted(_) | Error::Internal(_))
    }

    /// Build an I/O error with an optional path for context.
    pub fn io(path: Option<PathBuf>, source: io::Error) -> Self {
        Error::Io { path, source }
    }
}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        Error::Io { path: None, source }
    }
}

/// Convenience result alias used throughout the core.
pub type Result<T> = std::result::Result<T, Error>;
