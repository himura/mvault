//! Core library for the `mvault` command-line tool.

pub mod hash;
pub mod source;
pub mod store;

/// Runs the application.
///
/// # Errors
///
/// Returns an error if the application fails.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("Hello, world!");
    Ok(())
}
