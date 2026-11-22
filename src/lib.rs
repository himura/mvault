//! Core library for the `mvault` command-line tool.

/// Runs the application.
///
/// # Errors
///
/// Returns an error if the application fails.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("Hello, world!");
    Ok(())
}
