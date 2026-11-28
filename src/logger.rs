//! The detailed log that `--log` turns on.
//!
//! Only the records of mvault itself are written: those of the libraries it
//! uses, such as the HTTP client, may hold request headers and with them a
//! token.

use std::io::Write as _;

use log::{Level, LevelFilter, Log, Metadata, Record};

/// Writes the records of mvault to stderr, one line each.
#[derive(Debug)]
struct Stderr;

impl Log for Stderr {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        is_own(metadata.target()) && metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = match record.level() {
            Level::Error => "error",
            Level::Warn => "warning",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        };
        // The log is best effort: a closed stderr must not stop a command.
        let _ = writeln!(std::io::stderr().lock(), "{level}: {}", record.args());
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

fn is_own(target: &str) -> bool {
    target == "mvault" || target.starts_with("mvault::")
}

/// How much the log tells, as `--log` takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Detail {
    /// No log, as without --log
    Off,
    /// What is done and why
    Debug,
    /// Every step, also the headers of each response
    Trace,
}

/// The level of the log that `detail` asks for; off without it.
#[must_use]
pub(crate) fn level(detail: Option<Detail>) -> LevelFilter {
    match detail {
        None | Some(Detail::Off) => LevelFilter::Off,
        Some(Detail::Debug) => LevelFilter::Debug,
        Some(Detail::Trace) => LevelFilter::Trace,
    }
}

/// Starts writing the log at `level`. Does nothing when it is off, or when a
/// logger was set already.
pub(crate) fn init(level: LevelFilter) {
    if level == LevelFilter::Off {
        return;
    }
    if log::set_logger(&Stderr).is_ok() {
        log::set_max_level(level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_is_off_unless_asked_for() {
        assert_eq!(level(None), LevelFilter::Off);
        assert_eq!(level(Some(Detail::Off)), LevelFilter::Off);
        assert_eq!(level(Some(Detail::Debug)), LevelFilter::Debug);
        assert_eq!(level(Some(Detail::Trace)), LevelFilter::Trace);
    }

    #[test]
    fn only_the_records_of_mvault_are_written() {
        assert!(is_own("mvault"));
        assert!(is_own("mvault::download"));
        assert!(!is_own("mvaultish"));
        assert!(!is_own("ureq::unversioned"));
        assert!(!is_own("rustls::client"));
    }
}
