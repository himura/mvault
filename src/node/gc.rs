//! Removing the blobs of a node that nothing is linked to any more.

use std::fs::{self, File};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::sync::LINK_PREFIX;
use super::workspace::Workspace;
use crate::error::{Error, Result};
use crate::fsutil::io_error;
use crate::gc::{PARTIAL_MAX_AGE, blob_at, clean_tmp};
use crate::store::BlobStore;

/// How long a blob is kept after its last link went away, unless told
/// otherwise.
pub const DEFAULT_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// What a garbage collection removed, or would remove.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collected {
    /// Blobs no file is linked to, with their sizes, ordered by path.
    pub blobs: Vec<(PathBuf, u64)>,
    /// Partial files of downloads given up long ago.
    pub partials: Vec<PathBuf>,
    /// Temporary links left by a sync that stopped.
    pub links: Vec<PathBuf>,
    /// Files among the blobs that are not blobs: their name is not a
    /// digest, or they are not where the blob of that digest goes. They are
    /// left alone.
    pub strays: Vec<PathBuf>,
}

/// Removes the blobs that have no other link and have had none for at
/// least `min_age`, and what downloads and syncs left behind. With
/// `dry_run`, only reports, from the same decisions.
///
/// A placed file is a hard link to its blob, so a blob with a single link
/// is placed nowhere: not in this workspace, nor in any other that shares
/// the blobs. A change in the number of links updates the inode's change
/// time, which so tells when the last link went away.
///
/// # Errors
///
/// Returns an error if the blobs cannot be listed or removed, or on a
/// platform where the number of links of a file cannot be asked for.
pub fn gc(workspace: &Workspace, min_age: Duration, dry_run: bool) -> Result<Collected> {
    let store = &workspace.store;
    // Not while a sync is between fetching a blob and linking it.
    let _exclusive = if dry_run {
        None
    } else {
        Some(gc_lock(store, true)?)
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut files = store.blob_files().map_err(Error::Store)?;
    files.sort();
    let mut collected = Collected::default();
    for path in files {
        let Some(digest) = blob_at(store, &path) else {
            collected.strays.push(path);
            continue;
        };
        let metadata = fs::metadata(&path).map_err(io_error(&path))?;
        let (links, changed) = links(&metadata)?;
        if links > 1 || now.saturating_sub(changed) < min_age {
            continue;
        }
        if !dry_run {
            store.remove(&digest).map_err(Error::Store)?;
        }
        collected.blobs.push((path, metadata.len()));
    }
    collected.partials = clean_tmp(store.tmp_dir(), PARTIAL_MAX_AGE, dry_run)?;
    collected.links = clean_links(store, dry_run)?;
    Ok(collected)
}

/// Removes the temporary links a sync makes next to the blobs. Only called
/// with the exclusive lock held, when no sync runs, unless `dry_run`.
fn clean_links(store: &BlobStore, dry_run: bool) -> Result<Vec<PathBuf>> {
    let dir = store.tmp_dir();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error(dir)(error)),
    };
    let mut links = Vec::new();
    for entry in entries {
        let path = entry.map_err(io_error(dir))?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with(LINK_PREFIX) {
            links.push(path);
        }
    }
    links.sort();
    if !dry_run {
        for link in &links {
            fs::remove_file(link).map_err(io_error(link))?;
        }
    }
    Ok(links)
}

/// Takes the lock that keeps syncs and garbage collections apart: any
/// number of syncs share it, a collection has it alone.
pub(crate) fn gc_lock(store: &BlobStore, exclusive: bool) -> Result<File> {
    let dir = store.tmp_dir();
    fs::create_dir_all(dir).map_err(io_error(dir))?;
    let path = dir.join("gc.lock");
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_error(&path))?;
    if exclusive {
        file.lock().map_err(io_error(&path))?;
    } else {
        file.lock_shared().map_err(io_error(&path))?;
    }
    Ok(file)
}

/// The number of links of a file, and when it last changed (since the
/// epoch).
#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn links(metadata: &fs::Metadata) -> Result<(u64, Duration)> {
    use std::os::unix::fs::MetadataExt as _;
    let changed = u64::try_from(metadata.ctime()).map_or(Duration::ZERO, |secs| {
        Duration::new(secs, u32::try_from(metadata.ctime_nsec()).unwrap_or(0))
    });
    Ok((metadata.nlink(), changed))
}

#[cfg(not(unix))]
fn links(_metadata: &fs::Metadata) -> Result<(u64, Duration)> {
    Err(Error::Invalid(
        "gc is not supported on this platform: the number of links of a file cannot be asked for"
            .into(),
    ))
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::hash::Sha256;

    fn store_blob(workspace: &Workspace, content: &[u8]) -> PathBuf {
        let digest = Sha256::of(content);
        let partial = workspace.store.partial_path("new").unwrap();
        fs::write(&partial, content).unwrap();
        workspace.store.commit(&partial, &digest).unwrap();
        workspace.store.blob_path(&digest)
    }

    fn listing(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = walk(dir);
        paths.sort();
        paths
    }

    fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                paths.extend(walk(&path));
            }
            paths.push(path);
        }
        paths
    }

    #[test]
    fn blobs_without_another_link_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path().to_owned(), None).unwrap();
        let placed = store_blob(&workspace, b"placed");
        let unused = store_blob(&workspace, b"unused");
        fs::hard_link(&placed, dir.path().join("model.bin")).unwrap();

        let before = listing(dir.path());
        let would = gc(&workspace, Duration::ZERO, true).unwrap();
        assert_eq!(would.blobs, [(unused.clone(), 6)]);
        // A dry run changes nothing, not even the lock files.
        assert_eq!(listing(dir.path()), before);

        let collected = gc(&workspace, Duration::ZERO, false).unwrap();
        assert_eq!(collected.blobs, would.blobs);
        assert!(!unused.exists());
        assert!(placed.exists());
        assert_eq!(
            gc(&workspace, Duration::ZERO, false).unwrap(),
            Collected::default()
        );

        // Once the placed file is gone, so is the reason to keep the blob.
        fs::remove_file(dir.path().join("model.bin")).unwrap();
        assert_eq!(
            gc(&workspace, Duration::ZERO, false).unwrap().blobs,
            [(placed, 6)]
        );
    }

    #[test]
    fn a_blob_unlinked_recently_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path().to_owned(), None).unwrap();
        let blob = store_blob(&workspace, b"recent");

        assert_eq!(
            gc(&workspace, DEFAULT_MIN_AGE, false).unwrap().blobs,
            Vec::<(PathBuf, u64)>::new()
        );
        assert!(blob.exists());
        assert_eq!(
            gc(&workspace, Duration::ZERO, false).unwrap().blobs.len(),
            1
        );
    }

    #[test]
    fn files_that_are_not_blobs_are_reported_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path().to_owned(), None).unwrap();
        let blob = store_blob(&workspace, b"blob");
        let digest = Sha256::of(b"blob").to_string();
        // A digest in the wrong shard directory, and a name that is no
        // digest.
        let shards = blob.parent().unwrap().parent().unwrap();
        let wrong = shards.join(if digest.starts_with("00") { "01" } else { "00" });
        fs::create_dir_all(&wrong).unwrap();
        fs::copy(&blob, wrong.join(&digest)).unwrap();
        fs::write(blob.parent().unwrap().join("notes.txt"), b"x").unwrap();

        for dry_run in [true, false] {
            let collected = gc(&workspace, Duration::ZERO, dry_run).unwrap();
            let mut strays = collected.strays.clone();
            strays.sort();
            let mut expected = vec![
                wrong.join(&digest),
                blob.parent().unwrap().join("notes.txt"),
            ];
            expected.sort();
            assert_eq!(strays, expected);
        }
        assert!(wrong.join(&digest).exists());
        assert!(!blob.exists());
    }

    #[test]
    fn links_left_by_a_sync_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path().to_owned(), None).unwrap();
        let blob = store_blob(&workspace, b"linked");
        let left = workspace.store.tmp_dir().join(format!("{LINK_PREFIX}1-0"));
        fs::hard_link(&blob, &left).unwrap();

        let would = gc(&workspace, Duration::ZERO, true).unwrap();
        assert_eq!(would.links, [left.clone()]);
        // The link keeps the blob until it is gone.
        assert_eq!(would.blobs, Vec::<(PathBuf, u64)>::new());
        assert!(left.exists());

        assert_eq!(
            gc(&workspace, Duration::ZERO, false).unwrap().links,
            [left.clone()]
        );
        assert!(!left.exists());
        assert_eq!(
            gc(&workspace, Duration::ZERO, false).unwrap().blobs.len(),
            1
        );
    }

    #[test]
    fn an_empty_workspace_has_nothing_to_collect() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path().to_owned(), None).unwrap();
        assert_eq!(
            gc(&workspace, Duration::ZERO, false).unwrap(),
            Collected::default()
        );
    }
}
