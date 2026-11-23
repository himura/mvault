//! Small helpers for files written next to the blobs.

use std::fs;
use std::io;
use std::path::Path;

use crate::error::{Error, Result};

/// Wraps an I/O error with the path it is about.
pub(crate) fn io_error(path: &Path) -> impl FnOnce(io::Error) -> Error {
    let path = path.to_owned();
    move |source| Error::Io { path, source }
}

/// Replaces `path` with `content`, creating the directories above it.
///
/// Readers see either the old or the new content, never a part of it: the
/// content is written to a temporary file in the same directory and renamed.
pub(crate) fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(io_error(dir))?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    fs::write(&tmp, content).map_err(io_error(&tmp))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io_error(path)(e)
    })
}

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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_creates_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("file.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        let entries: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

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
