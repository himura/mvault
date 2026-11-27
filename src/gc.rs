//! What mirror and node have in common when they collect garbage.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::Result;
use crate::fsutil::{io_error, is_at};
use crate::hash::Sha256;
use crate::store::BlobStore;

/// How long an unfinished download is kept for a later attempt to continue.
pub const PARTIAL_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Removes what downloads left in `tmp`: partial files older than
/// `max_age`, and the lock files of downloads that are over. Whatever a
/// running download holds the lock of is left alone.
///
/// Returns the partial files removed, or with `dry_run` those that would
/// be.
///
/// # Errors
///
/// Returns an error if the directory cannot be listed or a file cannot be
/// removed.
pub fn clean_tmp(tmp: &Path, max_age: Duration, dry_run: bool) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let entries = match fs::read_dir(tmp) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(removed),
        Err(error) => return Err(io_error(tmp)(error)),
    };
    let mut partials = Vec::new();
    for entry in entries {
        let path = entry.map_err(io_error(tmp))?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with("partial-") && path.extension().is_none() {
            partials.push(path);
        } else if name.starts_with("partial-") && !path.with_extension("").exists() {
            // The lock file of a download that left no partial file.
            partials.push(path.with_extension(""));
        }
    }
    partials.sort();
    partials.dedup();

    for partial in partials {
        let lock_path = partial.with_extension("lock");
        // A dry run creates no lock file: where there is none, no download
        // is running.
        let lock = fs::File::options()
            .create(!dry_run)
            .truncate(false)
            .write(true)
            .open(&lock_path);
        let lock = match lock {
            Ok(lock) => Some(lock),
            Err(error) if dry_run && error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(io_error(&lock_path)(error)),
        };
        // A download in progress holds the lock; one that starts now waits
        // for it and then finds its lock file gone, and starts over.
        if let Some(lock) = &lock
            && (lock.try_lock().is_err() || !is_at(lock, &lock_path))
        {
            continue;
        }
        let stale = match fs::metadata(&partial) {
            Ok(metadata) => {
                let age = metadata
                    .modified()
                    .ok()
                    .and_then(|at| SystemTime::now().duration_since(at).ok());
                age.is_some_and(|age| age >= max_age)
            }
            Err(_) => false,
        };
        let exists = partial.exists();
        if exists && !stale {
            continue;
        }
        if exists {
            if !dry_run {
                fs::remove_file(&partial).map_err(io_error(&partial))?;
            }
            removed.push(partial);
        }
        if !dry_run {
            fs::remove_file(&lock_path).map_err(io_error(&lock_path))?;
        }
    }
    Ok(removed)
}

/// The digest of the blob at `path`, if `path` is where the store keeps
/// the blob of that digest. Any other file among the blobs is not a blob:
/// garbage collection reports it and leaves it alone.
#[must_use]
pub fn blob_at(store: &BlobStore, path: &Path) -> Option<Sha256> {
    let digest: Sha256 = path.file_name()?.to_str()?.parse().ok()?;
    (store.blob_path(&digest) == path).then_some(digest)
}

/// Reads a duration such as `90s`, `30m`, `12h`, `7d`, or `0`.
///
/// # Errors
///
/// Returns a message if `text` is not a whole number followed by one of the
/// units `s`, `m`, `h`, `d`.
pub fn parse_duration(text: &str) -> std::result::Result<Duration, String> {
    let invalid =
        || format!("invalid duration {text:?} (expected a number and s, m, h or d, such as 12h)");
    if text == "0" {
        return Ok(Duration::ZERO);
    }
    let unit = match text.chars().last() {
        Some('s') => 1,
        Some('m') => 60,
        Some('h') => 60 * 60,
        Some('d') => 24 * 60 * 60,
        _ => return Err(invalid()),
    };
    let digits = &text[..text.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let number: u64 = digits.parse().map_err(|_| invalid())?;
    number
        .checked_mul(unit)
        .map(Duration::from_secs)
        .ok_or_else(invalid)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::fsutil::lock_file;

    #[test]
    fn durations_have_a_unit() {
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("12h").unwrap(), Duration::from_secs(43200));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604_800));
        for text in [
            "",
            "1",
            "d",
            "1.5h",
            "-1d",
            "1w",
            "1 d",
            "99999999999999999999d",
        ] {
            assert!(parse_duration(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn old_partial_files_and_idle_lock_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str| dir.path().join(name);
        for name in [
            "partial-old",
            "partial-old.lock",
            "partial-new",
            "partial-new.lock",
        ] {
            fs::write(file(name), b"x").unwrap();
        }
        fs::write(file("partial-idle.lock"), b"").unwrap();
        fs::write(file("partial-bare"), b"x").unwrap();
        fs::write(file("other"), b"x").unwrap();
        let _held = lock_file(&file("partial-busy.lock")).unwrap();
        fs::write(file("partial-busy"), b"x").unwrap();

        // Everything was written just now: with a limit of zero it is all
        // old, with a long one nothing is.
        let kept = clean_tmp(dir.path(), Duration::from_secs(3600), false).unwrap();
        assert_eq!(kept, Vec::<PathBuf>::new());
        assert!(!file("partial-idle.lock").exists());
        assert!(file("partial-old.lock").exists());

        let listing = || {
            let mut names: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        let before = listing();
        let would = clean_tmp(dir.path(), Duration::ZERO, true).unwrap();
        // A dry run neither creates lock files nor removes any.
        assert_eq!(listing(), before);
        assert_eq!(
            would,
            [
                file("partial-bare"),
                file("partial-new"),
                file("partial-old")
            ]
        );
        assert!(file("partial-old").exists());

        let removed = clean_tmp(dir.path(), Duration::ZERO, false).unwrap();
        assert_eq!(removed, would);
        let mut left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["other", "partial-busy", "partial-busy.lock"]);
    }

    #[test]
    fn a_missing_directory_has_nothing_to_clean() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            clean_tmp(&dir.path().join("none"), Duration::ZERO, false).unwrap(),
            Vec::<PathBuf>::new()
        );
    }
}
