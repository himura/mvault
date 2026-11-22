use std::process::ExitCode;

fn main() -> ExitCode {
    match mvault::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
