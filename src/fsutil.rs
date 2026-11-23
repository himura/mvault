//! Small helpers for paths and files.

/// Checks that a path from outside (an API response, a command line) names a
/// file below the directory it will be joined to.
pub(crate) fn check_relative(path: &str) -> std::result::Result<(), String> {
    let safe = !path.is_empty()
        && !path.contains(['\\', '\0'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    if safe {
        Ok(())
    } else {
        Err(format!("unsafe path {path:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_stay_below_their_base() {
        for path in ["a", "a/b.txt", "a b/c", ".gitattributes", "a/..b"] {
            assert!(check_relative(path).is_ok(), "{path:?}");
        }
        for path in [
            "", "/a", "a/", "a//b", ".", "..", "a/../b", "a/./b", "a\\b", "a\0",
        ] {
            assert!(check_relative(path).is_err(), "{path:?}");
        }
    }
}
