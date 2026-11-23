//! Removing the blobs of a node that nothing is linked to any more.

use std::fs::{self, File};
use std::path::PathBuf;

use super::workspace::Workspace;
use crate::error::{Error, Result};
use crate::fsutil::io_error;
use crate::gc::{PARTIAL_MAX_AGE, clean_tmp};
use crate::store::BlobStore;

/// What a garbage collection removed, or would remove.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collected {
    /// Blobs no file is linked to, with their sizes, ordered by path.
    pub blobs: Vec<(PathBuf, u64)>,
    /// Partial files of downloads given up long ago.
    pub partials: Vec<PathBuf>,
}

/// Removes the blobs that have no other link, and what downloads left
/// behind. With `dry_run`, only reports.
///
/// A placed file is a hard link to its blob, so a blob with a single link
/// is placed nowhere: not in this workspace, nor in any other that shares
/// the blobs.
///
/// # Errors
///
/// Returns an error if the blobs cannot be listed or removed, or on a
/// platform where the number of links of a file cannot be asked for.
pub fn gc(workspace: &Workspace, dry_run: bool) -> Result<Collected> {
    let store = &workspace.store;
    // Not while a sync is between fetching a blob and linking it.
    let _exclusive = if dry_run {
        None
    } else {
        Some(gc_lock(store, true)?)
    };
    let mut files = store.blob_files().map_err(Error::Store)?;
    files.sort();
    let mut blobs = Vec::new();
    for path in files {
        let metadata = fs::metadata(&path).map_err(io_error(&path))?;
        if links(&metadata)? > 1 {
            continue;
        }
        if !dry_run {
            let digest = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .parse();
            match digest {
                Ok(digest) => store.remove(&digest).map_err(Error::Store)?,
                // Not a blob by its name; leave it alone.
                Err(_) => continue,
            }
        }
        blobs.push((path, metadata.len()));
    }
    let partials = clean_tmp(store.tmp_dir(), PARTIAL_MAX_AGE, dry_run)?;
    Ok(Collected { blobs, partials })
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

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn links(metadata: &fs::Metadata) -> Result<u64> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(metadata.nlink())
}

#[cfg(not(unix))]
fn links(_metadata: &fs::Metadata) -> Result<u64> {
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

    #[test]
    fn blobs_without_another_link_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path().to_owned(), None).unwrap();
        let placed = store_blob(&workspace, b"placed");
        let unused = store_blob(&workspace, b"unused");
        fs::hard_link(&placed, dir.path().join("model.bin")).unwrap();

        let would = gc(&workspace, true).unwrap();
        assert_eq!(would.blobs, [(unused.clone(), 6)]);
        assert!(unused.exists());

        let collected = gc(&workspace, false).unwrap();
        assert_eq!(collected.blobs, would.blobs);
        assert!(!unused.exists());
        assert!(placed.exists());
        assert_eq!(gc(&workspace, false).unwrap(), Collected::default());

        // Once the placed file is gone, so is the reason to keep the blob.
        fs::remove_file(dir.path().join("model.bin")).unwrap();
        assert_eq!(gc(&workspace, false).unwrap().blobs, [(placed, 6)]);
    }

    #[test]
    fn an_empty_workspace_has_nothing_to_collect() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path().to_owned(), None).unwrap();
        assert_eq!(gc(&workspace, false).unwrap(), Collected::default());
    }
}
