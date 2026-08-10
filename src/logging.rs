//! Structured logging setup with stdout and optional file output.

use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tracing::level_filters::LevelFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::error::{Error, Result};

/// A shared file writer suitable for the tracing fmt layer.
#[derive(Clone)]
struct SharedFileWriter {
    file: Arc<Mutex<std::fs::File>>,
}

impl io::Write for SharedFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file
            .lock()
            .map_err(|_| io::Error::other("log file mutex poisoned"))?
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file
            .lock()
            .map_err(|_| io::Error::other("log file mutex poisoned"))?
            .flush()
    }
}

impl<'a> MakeWriter<'a> for SharedFileWriter {
    type Writer = SharedFileWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Initialize tracing with a stdout layer and an optional file layer.
pub fn init(level: &str, file: Option<&Path>) -> Result<()> {
    let filter = parse_level(level)?;

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_writer(io::stdout)
        .with_target(false)
        .with_filter(filter);

    let registry = tracing_subscriber::registry().with(stdout_layer);

    if let Some(path) = file {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| Error::io(Some(path.to_path_buf()), e))?;
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(SharedFileWriter {
                file: Arc::new(Mutex::new(file)),
            })
            .with_ansi(false)
            .with_filter(filter);
        registry.with(file_layer).init();
    } else {
        registry.init();
    }
    Ok(())
}

fn parse_level(level: &str) -> Result<LevelFilter> {
    match level.trim().to_ascii_lowercase().as_str() {
        "trace" => Ok(LevelFilter::TRACE),
        "debug" => Ok(LevelFilter::DEBUG),
        "info" => Ok(LevelFilter::INFO),
        "warn" => Ok(LevelFilter::WARN),
        "error" => Ok(LevelFilter::ERROR),
        other => Err(Error::InvalidConfig(format!(
            "unknown log level {other:?}, expected trace|debug|info|warn|error"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_levels() {
        assert_eq!(parse_level("info").unwrap(), LevelFilter::INFO);
        assert_eq!(parse_level("TRACE").unwrap(), LevelFilter::TRACE);
        assert!(parse_level("loud").is_err());
    }
}
