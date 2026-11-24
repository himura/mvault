//! The record of placed files, `.mvault/placed.json`.
//!
//! Manifests and lock files reach a node from elsewhere, through version
//! control, so a file dropped from the manifest cannot be found from them
//! alone. `sync` writes down every file it places, and removes only what is
//! written down here.
//!
//! The record also keeps the size and modification time of each blob, to
//! notice that a blob was written to: a hard link shares its inode with the
//! blob, so whoever overwrites a placed file in place overwrites the blob.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read as _};
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fsutil::{check_relative, io_error, write_atomic};
use crate::hash::{Hasher, Sha256};

const SCHEMA: u32 = 1;

/// Name of the record in the state directory of a workspace.
pub const PLACED_NAME: &str = "placed.json";

/// The files `sync` placed in a workspace, by destination.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placed {
    files: BTreeMap<String, Entry>,
}

/// One placed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// The blob the file is a link to.
    pub sha256: Sha256,
    /// Size of the blob when it was recorded.
    pub size: u64,
    /// Modification time of the blob when it was recorded, in nanoseconds
    /// since the epoch.
    pub mtime_ns: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecord {
    schema: u32,
    files: Vec<RawEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    dest: String,
    sha256: Sha256,
    size: u64,
    mtime_ns: i64,
}

impl Placed {
    /// Reads the record at `path`. A missing file is an empty record.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or is not a record this
    /// version understands.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(io_error(path)(error)),
        };
        let invalid = |reason: String| Error::Invalid(format!("{}: {reason}", path.display()));
        let raw: RawRecord = serde_json::from_slice(&text).map_err(|e| invalid(e.to_string()))?;
        if raw.schema != SCHEMA {
            return Err(invalid(format!(
                "schema {} is not supported (expected {SCHEMA})",
                raw.schema
            )));
        }
        let mut files = BTreeMap::new();
        for entry in raw.files {
            check_relative(&entry.dest).map_err(invalid)?;
            files.insert(
                entry.dest,
                Entry {
                    sha256: entry.sha256,
                    size: entry.size,
                    mtime_ns: entry.mtime_ns,
                },
            );
        }
        Ok(Self { files })
    }

    /// Writes the record to `path`, replacing it as a whole.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<()> {
        let raw = RawRecord {
            schema: SCHEMA,
            files: self
                .files
                .iter()
                .map(|(dest, entry)| RawEntry {
                    dest: dest.clone(),
                    sha256: entry.sha256,
                    size: entry.size,
                    mtime_ns: entry.mtime_ns,
                })
                .collect(),
        };
        let mut json = serde_json::to_vec_pretty(&raw)
            .map_err(|e| Error::Invalid(format!("cannot write the record: {e}")))?;
        json.push(b'\n');
        write_atomic(path, &json)
    }

    /// The entry of the file at `dest`.
    #[must_use]
    pub fn get(&self, dest: &str) -> Option<&Entry> {
        self.files.get(dest)
    }

    /// Records the file at `dest`, replacing what was recorded for it.
    pub fn insert(&mut self, dest: String, entry: Entry) {
        self.files.insert(dest, entry);
    }

    /// Forgets the file at `dest`.
    pub fn remove(&mut self, dest: &str) -> Option<Entry> {
        self.files.remove(dest)
    }

    /// Every recorded file, ordered by destination.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Entry)> {
        self.files
            .iter()
            .map(|(dest, entry)| (dest.as_str(), entry))
    }

    /// Whether nothing is recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// What can be told about a blob without reading it: its size and
/// modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    /// Size in bytes.
    pub size: u64,
    /// Modification time in nanoseconds since the epoch.
    pub mtime_ns: i64,
}

impl Stamp {
    /// The stamp of the file at `path`, or `None` if there is none.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be looked at.
    pub fn of(path: &Path) -> Result<Option<Self>> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(path)(error)),
        };
        let mtime_ns = metadata
            .modified()
            .map_or(0, |at| match at.duration_since(UNIX_EPOCH) {
                Ok(after) => i64::try_from(after.as_nanos()).unwrap_or(i64::MAX),
                Err(before) => {
                    i64::try_from(before.duration().as_nanos()).map_or(i64::MIN, |nanos| -nanos)
                }
            });
        Ok(Some(Self {
            size: metadata.len(),
            mtime_ns,
        }))
    }

    /// The entry recording a link to the blob `sha256` with this stamp.
    #[must_use]
    pub fn entry(self, sha256: Sha256) -> Entry {
        Entry {
            sha256,
            size: self.size,
            mtime_ns: self.mtime_ns,
        }
    }

    /// Whether `entry` was recorded with this stamp.
    #[must_use]
    pub fn matches(self, entry: &Entry) -> bool {
        (self.size, self.mtime_ns) == (entry.size, entry.mtime_ns)
    }
}

/// The digest of the content of the file at `path`.
///
/// # Errors
///
/// Returns an error if the file cannot be read.
pub fn hash_file(path: &Path) -> Result<Sha256> {
    let mut file = fs::File::open(path).map_err(io_error(path))?;
    let mut hasher = Hasher::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io_error(path))?;
        if read == 0 {
            return Ok(hasher.finish());
        }
        hasher.update(&buffer[..read]);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn entry(content: &[u8]) -> Entry {
        Entry {
            sha256: Sha256::of(content),
            size: content.len() as u64,
            mtime_ns: 1_700_000_000_000_000_000,
        }
    }

    #[test]
    fn a_missing_record_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let placed = Placed::load(&dir.path().join(PLACED_NAME)).unwrap();
        assert!(placed.is_empty());
    }

    #[test]
    fn the_record_reads_back_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PLACED_NAME);
        let mut placed = Placed::default();
        placed.insert("models/b.bin".into(), entry(b"b"));
        placed.insert("models/a.bin".into(), entry(b"a"));
        placed.save(&path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.find("models/a.bin").unwrap() < text.find("models/b.bin").unwrap());
        assert!(text.contains("\"mtime_ns\": 1700000000000000000"), "{text}");
        let read = Placed::load(&path).unwrap();
        assert_eq!(read, placed);
        assert_eq!(read.get("models/a.bin"), Some(&entry(b"a")));
    }

    #[test]
    fn unusable_records_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PLACED_NAME);
        let digest = Sha256::of(b"a");
        for content in [
            String::new(),
            "{\"schema\": 2, \"files\": []}".to_owned(),
            format!(
                "{{\"schema\": 1, \"files\": [{{\"dest\": \"../x\", \"sha256\": \"{digest}\", \"size\": 1, \"mtime_ns\": 0}}]}}"
            ),
            "{\"schema\": 1, \"files\": [{\"dest\": \"x\", \"sha256\": \"abc\", \"size\": 1, \"mtime_ns\": 0}]}".to_owned(),
        ] {
            fs::write(&path, &content).unwrap();
            let error = Placed::load(&path).unwrap_err().to_string();
            assert!(error.contains(PLACED_NAME), "{content}: {error}");
        }
    }

    #[test]
    fn a_stamp_changes_with_the_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        assert_eq!(Stamp::of(&path).unwrap(), None);
        fs::write(&path, b"abc").unwrap();
        let stamp = Stamp::of(&path).unwrap().unwrap();
        assert_eq!(stamp.size, 3);
        assert!(stamp.matches(&stamp.entry(Sha256::of(b"abc"))));
        fs::write(&path, b"abcd").unwrap();
        assert!(
            !Stamp::of(&path)
                .unwrap()
                .unwrap()
                .matches(&stamp.entry(Sha256::of(b"abc")))
        );
        assert_eq!(hash_file(&path).unwrap(), Sha256::of(b"abcd"));
    }
}
