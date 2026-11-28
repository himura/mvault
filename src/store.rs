//! Content-addressed storage of blobs.
//!
//! A blob lives at `<blobs>/sha256/<first two hex digits>/<digest>` and is
//! read-only from the moment it appears there. The mirror and the nodes use
//! the same layout, so a node can copy a blob by path alone.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use log::debug;

use crate::fsutil::sync_dir;
use crate::hash::Sha256;

/// A directory of blobs and the directory where they are written before they
/// are complete.
///
/// Both directories must be on the same filesystem: a finished file enters
/// the store by a rename.
#[derive(Debug, Clone)]
pub struct BlobStore {
    blobs: PathBuf,
    tmp: PathBuf,
}

/// What [`BlobStore::commit`] did with the file it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Committed {
    /// The file became a new blob.
    Added,
    /// A blob with this digest was already stored; the file was removed.
    AlreadyPresent,
}

impl BlobStore {
    /// A store with blobs under `blobs` and unfinished files under `tmp`.
    ///
    /// Nothing is created until it is needed.
    pub fn new(blobs: impl Into<PathBuf>, tmp: impl Into<PathBuf>) -> Self {
        Self {
            blobs: blobs.into(),
            tmp: tmp.into(),
        }
    }

    /// The path of the blob with this digest, relative to the blobs directory.
    ///
    /// This is also its path below the root of a mirror's public tree.
    #[must_use]
    pub fn relative_path(digest: &Sha256) -> PathBuf {
        let hex = digest.to_string();
        Path::new("sha256").join(&hex[..2]).join(&hex)
    }

    /// The path the blob with this digest has, whether or not it is stored.
    #[must_use]
    pub fn blob_path(&self, digest: &Sha256) -> PathBuf {
        self.blobs.join(Self::relative_path(digest))
    }

    /// Whether the blob with this digest is stored.
    #[must_use]
    pub fn contains(&self, digest: &Sha256) -> bool {
        self.blob_path(digest).is_file()
    }

    /// Whether the blob with this digest is stored, telling a blob that is
    /// absent from one that cannot be looked at.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be looked at for another reason
    /// than that it does not exist.
    pub fn find(&self, digest: &Sha256) -> io::Result<bool> {
        let path = self.blob_path(digest);
        match fs::metadata(&path) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(with_path(&error, &path)),
        }
    }

    /// Checks that the directory of the blobs is there, so that a store
    /// that is not mounted is not taken for one whose blobs are gone.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory is missing or cannot be looked at.
    pub fn check_present(&self) -> io::Result<()> {
        let root = self.blobs.join("sha256");
        match fs::metadata(&root) {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a directory", root.display()),
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not there (is it mounted?)", root.display()),
            )),
            Err(error) => Err(with_path(&error, &root)),
        }
    }

    /// The directory of unfinished files.
    #[must_use]
    pub fn tmp_dir(&self) -> &Path {
        &self.tmp
    }

    /// Removes the blob with this digest. A blob that is not stored is not
    /// an error.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be removed.
    pub fn remove(&self, digest: &Sha256) -> io::Result<()> {
        let path = self.blob_path(digest);
        debug!("removing blob {}", path.display());
        let removed = fs::remove_file(&path).or_else(|error| {
            // On Unix whether a file can be removed depends on its directory
            // alone. Its mode is shared with every file placed as a link to
            // it, so it is not touched.
            if cfg!(unix) || error.kind() != io::ErrorKind::PermissionDenied {
                return Err(error);
            }
            // Where a read-only file cannot be removed as it is.
            make_writable(&path)?;
            fs::remove_file(&path)
        });
        match removed {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(with_path(&error, &path)),
            _ => Ok(()),
        }
    }

    /// The paths of every stored blob, in no particular order.
    ///
    /// # Errors
    ///
    /// Returns an error if the directories cannot be listed.
    pub fn blob_files(&self) -> io::Result<Vec<PathBuf>> {
        let root = self.blobs.join("sha256");
        let mut files = Vec::new();
        let shards = match fs::read_dir(&root) {
            Ok(shards) => shards,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(files),
            Err(error) => return Err(with_path(&error, &root)),
        };
        for shard in shards {
            let shard = shard.map_err(|e| with_path(&e, &root))?.path();
            if !shard.is_dir() {
                continue;
            }
            for blob in fs::read_dir(&shard).map_err(|e| with_path(&e, &shard))? {
                files.push(blob.map_err(|e| with_path(&e, &shard))?.path());
            }
        }
        Ok(files)
    }

    /// The path for an unfinished file named by `key`, creating the directory
    /// for it. The same key gives the same path, so that an interrupted
    /// download can be continued.
    ///
    /// # Errors
    ///
    /// Returns an error if `key` is not a plain file name made of ASCII
    /// letters, digits, `.`, `-` and `_`, or if the directory cannot be
    /// created.
    pub fn partial_path(&self, key: &str) -> io::Result<PathBuf> {
        let plain = !key.is_empty()
            && !key.starts_with('.')
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
        if !plain {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid key for a partial file: {key:?}"),
            ));
        }
        fs::create_dir_all(&self.tmp).map_err(|e| with_path(&e, &self.tmp))?;
        Ok(self.tmp.join(format!("partial-{key}")))
    }

    /// Moves a completely written file into the store as the blob `digest`.
    ///
    /// The caller vouches that `digest` is the digest of the file's content;
    /// it is expected to have computed it while writing the file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be flushed to disk, made
    /// read-only or renamed into place.
    pub fn commit(&self, file: &Path, digest: &Sha256) -> io::Result<Committed> {
        self.prepare(file, digest)?.commit()
    }

    /// Does the slow part of [`BlobStore::commit`]: flushes the file to disk
    /// and creates the directory it goes to. What is left for
    /// [`Prepared::commit`] is quick, so that it can run while a lock is
    /// held that others wait for.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be flushed to disk or the
    /// directory cannot be created.
    pub fn prepare(&self, file: &Path, digest: &Sha256) -> io::Result<Prepared> {
        let dest = self.blob_path(digest);
        if !dest.is_file() {
            // Opened for writing: not every platform can flush a read-only
            // handle.
            File::options()
                .write(true)
                .open(file)
                .and_then(|f| f.sync_all())
                .map_err(|e| with_path(&e, file))?;
            let dir = dest.parent().unwrap_or(&self.blobs);
            create_dir_all_synced(dir)?;
        }
        Ok(Prepared {
            file: file.to_owned(),
            dest,
        })
    }
}

/// A file that is flushed to disk and ready to become a blob.
#[derive(Debug)]
#[must_use = "the file is not a blob until it is committed"]
pub struct Prepared {
    file: PathBuf,
    dest: PathBuf,
}

impl Prepared {
    /// Makes the file a blob: read-only, at its place in the store.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be made read-only or renamed
    /// into place.
    pub fn commit(self) -> io::Result<Committed> {
        let Self { file, dest } = self;
        // Another writer may have stored the blob since the file was
        // prepared.
        if dest.is_file() {
            debug!(
                "{} is stored already; removing {}",
                dest.display(),
                file.display()
            );
            fs::remove_file(&file).map_err(|e| with_path(&e, &file))?;
            return Ok(Committed::AlreadyPresent);
        }
        make_read_only(&file).map_err(|e| with_path(&e, &file))?;
        if let Err(e) = place(&file, &dest) {
            // Losing the race against another writer of the same blob ends
            // up here: its blob stays as it is.
            if dest.is_file() {
                debug!(
                    "{} was stored by another writer meanwhile; removing {}",
                    dest.display(),
                    file.display()
                );
                fs::remove_file(&file).map_err(|e| with_path(&e, &file))?;
                return Ok(Committed::AlreadyPresent);
            }
            // The file stays a partial file, which a resumed download or a
            // second commit has to be able to open for writing.
            let _ = make_writable(&file);
            return Err(with_path(&e, &dest));
        }
        if let Some(dir) = dest.parent() {
            sync_dir(dir).map_err(|e| with_path(&e, dir))?;
        }
        debug!("stored {} as {}", file.display(), dest.display());
        Ok(Committed::Added)
    }
}

/// Gives `file` the name `dest`, unless something has that name already.
///
/// A blob stored meanwhile by another writer must not be replaced: the
/// files placed as links to it would no longer be the blob. A hard link
/// fails where its name is taken; the name `file` is removed afterwards.
#[cfg(unix)]
fn place(file: &Path, dest: &Path) -> io::Result<()> {
    fs::hard_link(file, dest)?;
    // The blob is in place. Were the old name left behind, it would be
    // just another link to the blob: no download writes to a partial file
    // whose content is stored already, and garbage collection removes it.
    let _ = fs::remove_file(file);
    Ok(())
}

/// Where a rename does not replace a read-only file, as a blob is.
#[cfg(not(unix))]
fn place(file: &Path, dest: &Path) -> io::Result<()> {
    fs::rename(file, dest)
}

#[cfg(unix)]
fn make_read_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o444))
}

#[cfg(not(unix))]
fn make_read_only(path: &Path) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
}

#[cfg(unix)]
fn make_writable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = fs::metadata(path)?.permissions().mode();
    fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200))
}

#[cfg(not(unix))]
fn make_writable(path: &Path) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)
}

/// Creates `dir` and the directories above it that are missing, and makes
/// each new entry durable by flushing the directory that holds it. Without
/// that, a power loss could take a new shard directory, and every blob
/// renamed into it, with it.
fn create_dir_all_synced(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let parent = dir.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        create_dir_all_synced(parent)?;
    }
    match fs::create_dir(dir) {
        Ok(()) => {}
        // Created meanwhile by another writer, who flushes it.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => return Ok(()),
        Err(e) => return Err(with_path(&e, dir)),
    }
    if let Some(parent) = parent {
        sync_dir(parent).map_err(|e| with_path(&e, parent))?;
    }
    Ok(())
}

/// The same error, naming the path it is about.
fn with_path(error: &io::Error, path: &Path) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn store(dir: &Path) -> BlobStore {
        BlobStore::new(dir.join("blobs"), dir.join("tmp"))
    }

    fn write_partial(store: &BlobStore, key: &str, content: &[u8]) -> PathBuf {
        let path = store.partial_path(key).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn blob_paths_are_sharded_by_the_first_byte() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let digest = Sha256::of(b"abc");
        assert_eq!(
            BlobStore::relative_path(&digest),
            Path::new("sha256").join("ba").join(ABC)
        );
        assert_eq!(
            store.blob_path(&digest),
            dir.path().join("blobs").join("sha256").join("ba").join(ABC)
        );
    }

    #[test]
    fn commit_moves_the_file_and_makes_it_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let digest = Sha256::of(b"abc");
        assert!(!store.contains(&digest));

        let partial = write_partial(&store, "one", b"abc");
        assert_eq!(store.commit(&partial, &digest).unwrap(), Committed::Added);

        assert!(!partial.exists());
        assert!(store.contains(&digest));
        let blob = store.blob_path(&digest);
        assert_eq!(fs::read(&blob).unwrap(), b"abc");
        assert!(fs::metadata(&blob).unwrap().permissions().readonly());
    }

    #[test]
    fn commit_of_a_stored_digest_drops_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let digest = Sha256::of(b"abc");
        store
            .commit(&write_partial(&store, "one", b"abc"), &digest)
            .unwrap();

        let second = write_partial(&store, "two", b"abc");
        assert_eq!(
            store.commit(&second, &digest).unwrap(),
            Committed::AlreadyPresent
        );
        assert!(!second.exists());
        assert_eq!(fs::read(store.blob_path(&digest)).unwrap(), b"abc");
    }

    #[test]
    fn a_prepared_file_stays_writable_until_it_is_committed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let digest = Sha256::of(b"abc");
        let partial = write_partial(&store, "one", b"abc");

        let prepared = store.prepare(&partial, &digest).unwrap();

        assert!(partial.exists());
        assert!(!fs::metadata(&partial).unwrap().permissions().readonly());
        assert!(!store.contains(&digest));
        assert_eq!(prepared.commit().unwrap(), Committed::Added);
        assert!(!partial.exists());
        assert_eq!(fs::read(store.blob_path(&digest)).unwrap(), b"abc");
    }

    #[test]
    fn committing_a_prepared_file_yields_to_a_blob_stored_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let digest = Sha256::of(b"abc");
        let first = write_partial(&store, "one", b"abc");
        let second = write_partial(&store, "two", b"abc");

        let prepared = store.prepare(&second, &digest).unwrap();
        store.commit(&first, &digest).unwrap();

        assert_eq!(prepared.commit().unwrap(), Committed::AlreadyPresent);
        assert!(!second.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_blob_that_cannot_be_removed_stays_read_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let digest = Sha256::of(b"abc");
        store
            .commit(&write_partial(&store, "one", b"abc"), &digest)
            .unwrap();
        let blob = store.blob_path(&digest);
        let shard = blob.parent().unwrap().to_owned();
        fs::set_permissions(&shard, fs::Permissions::from_mode(0o555)).unwrap();

        let result = store.remove(&digest);

        fs::set_permissions(&shard, fs::Permissions::from_mode(0o755)).unwrap();
        if result.is_ok() {
            // Run by root, who may remove it all the same.
            return;
        }
        let mode = fs::metadata(&blob).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o444);
    }

    #[cfg(unix)]
    #[test]
    fn placing_a_file_never_replaces_a_stored_blob() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let digest = Sha256::of(b"abc");
        store
            .commit(&write_partial(&store, "one", b"abc"), &digest)
            .unwrap();
        let blob = store.blob_path(&digest);
        let placed = dir.path().join("placed");
        fs::hard_link(&blob, &placed).unwrap();
        // As when another writer stores the blob after commit looked.
        let late = write_partial(&store, "two", b"abc");

        let error = place(&late, &blob).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(same_file::is_same_file(&placed, &blob).unwrap());
        assert!(late.exists());
    }

    #[test]
    fn a_failed_commit_leaves_the_file_writable() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let digest = Sha256::of(b"abc");
        let partial = write_partial(&store, "one", b"abc");
        // A non-empty directory where the blob goes makes the rename fail.
        let blob = store.blob_path(&digest);
        fs::create_dir_all(blob.join("in-the-way")).unwrap();

        assert!(store.commit(&partial, &digest).is_err());

        assert!(!fs::metadata(&partial).unwrap().permissions().readonly());
        fs::OpenOptions::new().append(true).open(&partial).unwrap();
        fs::remove_dir_all(&blob).unwrap();
        assert_eq!(store.commit(&partial, &digest).unwrap(), Committed::Added);
    }

    #[test]
    fn commit_creates_the_missing_directories_above_the_blob() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(
            dir.path().join("a").join("b").join("blobs"),
            dir.path().join("tmp"),
        );
        let digest = Sha256::of(b"abc");
        let partial = write_partial(&store, "one", b"abc");

        assert_eq!(store.commit(&partial, &digest).unwrap(), Committed::Added);
        assert_eq!(fs::read(store.blob_path(&digest)).unwrap(), b"abc");
    }

    #[test]
    fn commit_reports_the_path_of_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let missing = dir.path().join("tmp").join("partial-missing");
        let err = store.commit(&missing, &Sha256::of(b"abc")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("partial-missing"), "{err}");
    }

    #[test]
    fn partial_paths_are_stable_and_inside_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let path = store.partial_path(ABC).unwrap();
        assert_eq!(path, dir.path().join("tmp").join(format!("partial-{ABC}")));
        assert_eq!(store.partial_path(ABC).unwrap(), path);
        assert!(dir.path().join("tmp").is_dir());
    }

    #[test]
    fn partial_keys_must_be_plain_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        for key in ["", ".", "..", ".hidden", "a/b", "a\\b", "a b", "é"] {
            let err = store.partial_path(key).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{key:?}");
        }
    }
}
