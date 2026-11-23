//! Core library for the `mvault` command-line tool.

pub mod download;
pub mod error;
pub mod hash;
pub mod mirror;
pub mod source;
pub mod store;
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod testutil;

/// Runs the application.
///
/// # Errors
///
/// Returns an error if the application fails.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("Hello, world!");
    Ok(())
}
