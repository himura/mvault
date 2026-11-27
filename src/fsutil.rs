//! Small helpers for files written next to the blobs.

use std::fs;
use std::io::{self, Write as _};
use std::path::Path;

use crate::error::{Error, Result};

/// Wraps an I/O error with the path it is about.
pub(crate) fn io_error(path: &Path) -> impl FnOnce(io::Error) -> Error {
    let path = path.to_owned();
    move |source| Error::Io { path, source }
}

/// Opens the lock file at `path`, creating it, and waits for the exclusive
/// lock on it. The lock is released when the returned file is dropped.
///
/// A lock file may be removed by whoever holds its lock. A lock taken on a
/// file that was removed meanwhile protects nothing, so the lock only
/// counts if the file is still the one at `path`.
pub(crate) fn lock_file(path: &Path) -> Result<fs::File> {
    loop {
        let file = fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(io_error(path))?;
        file.lock().map_err(io_error(path))?;
        if is_at(&file, path) {
            return Ok(file);
        }
    }
}

/// Whether the open `file` is the file at `path` right now.
pub(crate) fn is_at(file: &fs::File, path: &Path) -> bool {
    let open = file.try_clone().and_then(same_file::Handle::from_file);
    let named = same_file::Handle::from_path(path);
    matches!((open, named), (Ok(open), Ok(named)) if open == named)
}

/// Replaces `path` with `content`, creating the directories above it.
///
/// Readers see either the old or the new content, never a part of it: the
/// content is written to a temporary file in the same directory and renamed.
///
/// The content is flushed to disk before the rename, and the rename after
/// it, so that a power loss leaves the old content or the new, and not an
/// empty file that no command could read any more.
pub(crate) fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(io_error(dir))?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let written = fs::File::create(&tmp)
        .and_then(|mut file| {
            file.write_all(content)?;
            file.sync_all()
        })
        .map_err(io_error(&tmp));
    let renamed = written.and_then(|()| fs::rename(&tmp, path).map_err(io_error(path)));
    if renamed.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    renamed?;
    sync_dir(dir).map_err(io_error(dir))
}

/// Makes the creation, removal or rename of an entry of `dir` durable.
#[cfg(unix)]
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Elsewhere a directory cannot be opened to be flushed.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Checks that a path from outside (an API response, a command line) names a
/// file below the directory it will be joined to.
///
/// It must not lead into a directory of git either: a `.git/config` placed
/// there runs commands of its own choosing when git is run in that
/// directory. Control characters are refused, as they would also be
/// printed to the terminal.
pub(crate) fn check_relative(path: &str) -> std::result::Result<(), String> {
    let safe = !path.is_empty()
        && !path.contains(['\\'])
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && !is_git_dir(part));
    if safe {
        Ok(())
    } else {
        Err(format!("unsafe path {path:?}"))
    }
}

/// Whether a file name is one that git takes for `.git`, on any platform:
/// in any case, with trailing dots or spaces, or as its short DOS name.
pub(crate) fn is_git_dir(name: &str) -> bool {
    let name = name.trim_end_matches(['.', ' ']);
    name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case("git~1")
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
        for path in [
            "a",
            "a/b.txt",
            "a b/c",
            ".gitattributes",
            ".github/x",
            "a/..b",
            "a.git/b",
        ] {
            assert!(check_relative(path).is_ok(), "{path:?}");
        }
        for path in [
            "",
            "/a",
            "a/",
            "a//b",
            ".",
            "..",
            "a/../b",
            "a/./b",
            "a\\b",
            "a\0",
            "a\nb",
            "a\u{1b}[31m",
            ".git",
            ".git/config",
            "models/x/.git/config",
            ".GIT/config",
            ".git./config",
            ".git /config",
            "GIT~1/config",
        ] {
            assert!(check_relative(path).is_err(), "{path:?}");
        }
    }
}
