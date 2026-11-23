//! The state of a mirror: which snapshots it has and which blobs they use.
//!
//! The database is only ever opened on the mirror host itself. Everything a
//! node reads is written out from here as static files.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};

use crate::error::{Error, Result};
use crate::hash::Sha256;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE blobs (
    sha256      TEXT PRIMARY KEY,
    size        INTEGER NOT NULL,
    created_at  INTEGER NOT NULL
) STRICT;

CREATE TABLE snapshots (
    id          INTEGER PRIMARY KEY,
    source      TEXT NOT NULL,
    revision    TEXT NOT NULL,
    fetched_at  INTEGER NOT NULL,
    UNIQUE (source, revision)
) STRICT;

-- file_id is 0 for sources whose files are identified by path alone.
-- sha256 is what the upstream announced until the file is fetched, and the
-- digest of the stored blob from then on.
CREATE TABLE snapshot_files (
    snapshot_id INTEGER NOT NULL REFERENCES snapshots (id) ON DELETE CASCADE,
    path        TEXT NOT NULL,
    file_id     INTEGER NOT NULL DEFAULT 0,
    size        INTEGER,
    sha256      TEXT,
    fetched     INTEGER NOT NULL DEFAULT 0,
    is_primary  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (snapshot_id, path, file_id)
) STRICT;

CREATE INDEX snapshot_files_sha256 ON snapshot_files (sha256);

CREATE TABLE refs (
    source      TEXT NOT NULL,
    name        TEXT NOT NULL,
    revision    TEXT NOT NULL,
    PRIMARY KEY (source, name)
) STRICT;
";

/// A file of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Path in the repository, or the file name where files have ids.
    pub path: String,
    /// Id of the file, where the upstream gives files ids.
    pub file_id: Option<u64>,
    /// Size in bytes, if known.
    pub size: Option<u64>,
    /// Digest, if the upstream announced it or the file has been fetched.
    pub sha256: Option<Sha256>,
    /// Whether the upstream marks this as the main file of the snapshot.
    pub primary: bool,
    /// Whether the mirror holds the content as a blob.
    pub fetched: bool,
}

/// What identifies a file within a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileKey<'a> {
    /// Path of the file.
    pub path: &'a str,
    /// Id of the file, where the upstream gives files ids.
    pub file_id: Option<u64>,
}

impl FileEntry {
    /// The key of this file.
    #[must_use]
    pub fn key(&self) -> FileKey<'_> {
        FileKey {
            path: &self.path,
            file_id: self.file_id,
        }
    }
}

/// The database of a mirror.
#[derive(Debug)]
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Opens the database at `path`, creating it if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened as a database, or if it
    /// was written by a newer version of this program.
    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        // Other processes may hold the write lock for as long as a rename
        // and a few statements take; wait instead of failing.
        conn.busy_timeout(Duration::from_secs(60))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;

        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match version {
            0 => {
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            other => {
                return Err(Error::Invalid(format!(
                    "{}: database schema version {other} is not supported (expected {SCHEMA_VERSION})",
                    path.display()
                )));
            }
        }
        tx.commit()?;
        Ok(Self { conn })
    }

    /// Records a snapshot and the files the upstream lists for it, and
    /// returns its id.
    ///
    /// A snapshot that is already recorded keeps what it has fetched; files
    /// that are new to it are added.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be written.
    pub fn record_snapshot(
        &mut self,
        source: &str,
        revision: &str,
        files: &[FileEntry],
    ) -> Result<i64> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO snapshots (source, revision, fetched_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (source, revision) DO UPDATE SET fetched_at = excluded.fetched_at",
            params![source, revision, now()],
        )?;
        let id: i64 = tx.query_row(
            "SELECT id FROM snapshots WHERE source = ?1 AND revision = ?2",
            params![source, revision],
            |row| row.get(0),
        )?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO snapshot_files (snapshot_id, path, file_id, size, sha256, is_primary)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (snapshot_id, path, file_id) DO UPDATE SET
                     is_primary = excluded.is_primary,
                     size = CASE WHEN fetched THEN size ELSE excluded.size END,
                     sha256 = CASE WHEN fetched THEN sha256 ELSE excluded.sha256 END",
            )?;
            for file in files {
                insert.execute(params![
                    id,
                    file.path,
                    file.file_id.unwrap_or(0),
                    file.size,
                    file.sha256.map(|digest| digest.to_string()),
                    file.primary,
                ])?;
            }
        }
        tx.commit()?;
        Ok(id)
    }

    /// The files of a snapshot, ordered by path and file id.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be read or holds a digest
    /// that is not one.
    pub fn snapshot_files(&self, snapshot_id: i64) -> Result<Vec<FileEntry>> {
        let mut select = self.conn.prepare(
            "SELECT path, file_id, size, sha256, is_primary, fetched FROM snapshot_files
             WHERE snapshot_id = ?1 ORDER BY path, file_id",
        )?;
        let rows = select.query_map([snapshot_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, Option<u64>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, bool>(5)?,
            ))
        })?;
        let mut files = Vec::new();
        for row in rows {
            let (path, file_id, size, sha256, primary, fetched) = row?;
            let sha256 = sha256
                .map(|hex| hex.parse::<Sha256>())
                .transpose()
                .map_err(|e| Error::Invalid(format!("database: {e}")))?;
            files.push(FileEntry {
                path,
                file_id: (file_id != 0).then_some(file_id),
                size,
                sha256,
                primary,
                fetched,
            });
        }
        Ok(files)
    }

    /// Points a file of a snapshot at a blob the mirror already holds.
    ///
    /// Returns `false`, changing nothing, if there is no such blob.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be written or the snapshot
    /// has no such file.
    pub fn use_stored_blob(
        &mut self,
        snapshot_id: i64,
        file: FileKey<'_>,
        sha256: &Sha256,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let size: Option<u64> = tx
            .query_row(
                "SELECT size FROM blobs WHERE sha256 = ?1",
                [sha256.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(size) = size else {
            return Ok(false);
        };
        mark_fetched(&tx, snapshot_id, file, sha256, size)?;
        tx.commit()?;
        Ok(true)
    }

    /// Records a new blob as the content of a file of a snapshot.
    ///
    /// `place` moves the blob into the store. It runs while this process
    /// holds the write lock of the database, so that the blob appears on disk
    /// and in the database as one step with respect to other writers; if it
    /// fails, nothing is recorded.
    ///
    /// # Errors
    ///
    /// Returns the error of `place`, or an error if the database cannot be
    /// written or the snapshot has no such file.
    pub fn record_blob(
        &mut self,
        snapshot_id: i64,
        file: FileKey<'_>,
        sha256: &Sha256,
        size: u64,
        place: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        place()?;
        tx.execute(
            "INSERT INTO blobs (sha256, size, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (sha256) DO NOTHING",
            params![sha256.to_string(), size, now()],
        )?;
        mark_fetched(&tx, snapshot_id, file, sha256, size)?;
        tx.commit()?;
        Ok(())
    }

    /// Makes `name` (a branch, a tag, "latest") stand for `revision`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be written.
    pub fn set_ref(&mut self, source: &str, name: &str, revision: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO refs (source, name, revision) VALUES (?1, ?2, ?3)
             ON CONFLICT (source, name) DO UPDATE SET revision = excluded.revision",
            params![source, name, revision],
        )?;
        Ok(())
    }

    /// The names that stand for revisions of `source`, ordered by name.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be read.
    pub fn refs(&self, source: &str) -> Result<Vec<(String, String)>> {
        let mut select = self
            .conn
            .prepare("SELECT name, revision FROM refs WHERE source = ?1 ORDER BY name")?;
        let rows = select.query_map([source], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn mark_fetched(
    tx: &rusqlite::Transaction<'_>,
    snapshot_id: i64,
    file: FileKey<'_>,
    sha256: &Sha256,
    size: u64,
) -> Result<()> {
    let changed = tx.execute(
        "UPDATE snapshot_files SET fetched = 1, sha256 = ?4, size = ?5
         WHERE snapshot_id = ?1 AND path = ?2 AND file_id = ?3",
        params![
            snapshot_id,
            file.path,
            file.file_id.unwrap_or(0),
            sha256.to_string(),
            size
        ],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "snapshot {snapshot_id} has no file {:?}",
            file.path
        )))
    }
}

/// Seconds since the Unix epoch.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const SOURCE: &str = "hf:huggingface.co:example-org/example-model";

    fn open(dir: &Path) -> Db {
        Db::open(&dir.join("mvault.db")).unwrap()
    }

    fn entry(path: &str, sha256: Option<Sha256>) -> FileEntry {
        FileEntry {
            path: path.into(),
            file_id: None,
            size: Some(3),
            sha256,
            primary: false,
            fetched: false,
        }
    }

    #[test]
    fn a_snapshot_lists_its_files_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(dir.path());
        let files = [
            entry("b.bin", Some(Sha256::of(b"b"))),
            entry("a.json", None),
        ];

        let id = db.record_snapshot(SOURCE, "commit1", &files).unwrap();

        assert_eq!(
            db.snapshot_files(id).unwrap(),
            [files[1].clone(), files[0].clone()]
        );
        assert_eq!(db.snapshot_files(id + 1).unwrap(), []);
    }

    #[test]
    fn files_with_ids_may_share_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(dir.path());
        let file = |file_id, primary| FileEntry {
            file_id: Some(file_id),
            primary,
            ..entry("model.safetensors", None)
        };
        let files = [file(111, true), file(222, false)];

        let id = db
            .record_snapshot("civitai:example.com:12345", "67890", &files)
            .unwrap();

        assert_eq!(db.snapshot_files(id).unwrap(), files);
    }

    #[test]
    fn recording_a_blob_marks_the_file_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(dir.path());
        let id = db
            .record_snapshot(SOURCE, "commit1", &[entry("a.json", None)])
            .unwrap();
        let digest = Sha256::of(b"abc");
        let key = FileKey {
            path: "a.json",
            file_id: None,
        };

        let mut placed = false;
        db.record_blob(id, key, &digest, 3, || {
            placed = true;
            Ok(())
        })
        .unwrap();

        assert!(placed);
        let files = db.snapshot_files(id).unwrap();
        assert!(files[0].fetched);
        assert_eq!(files[0].sha256, Some(digest));
    }

    #[test]
    fn a_failed_placement_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(dir.path());
        let id = db
            .record_snapshot(SOURCE, "commit1", &[entry("a.json", None)])
            .unwrap();
        let digest = Sha256::of(b"abc");
        let key = FileKey {
            path: "a.json",
            file_id: None,
        };

        let result = db.record_blob(id, key, &digest, 3, || Err(Error::Invalid("no".into())));

        assert!(result.is_err());
        assert!(!db.snapshot_files(id).unwrap()[0].fetched);
        assert!(!db.use_stored_blob(id, key, &digest).unwrap());
    }

    #[test]
    fn a_stored_blob_is_shared_between_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(dir.path());
        let digest = Sha256::of(b"abc");
        let key = FileKey {
            path: "a.bin",
            file_id: None,
        };
        let first = db
            .record_snapshot(SOURCE, "commit1", &[entry("a.bin", Some(digest))])
            .unwrap();
        let second = db
            .record_snapshot(SOURCE, "commit2", &[entry("a.bin", Some(digest))])
            .unwrap();
        assert!(!db.use_stored_blob(second, key, &digest).unwrap());

        db.record_blob(first, key, &digest, 3, || Ok(())).unwrap();

        assert!(db.use_stored_blob(second, key, &digest).unwrap());
        assert!(db.snapshot_files(second).unwrap()[0].fetched);
    }

    #[test]
    fn recording_a_snapshot_again_keeps_what_was_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(dir.path());
        let files = [entry("a.json", None)];
        let id = db.record_snapshot(SOURCE, "commit1", &files).unwrap();
        let digest = Sha256::of(b"abc");
        db.record_blob(id, files[0].key(), &digest, 3, || Ok(()))
            .unwrap();

        let again = db
            .record_snapshot(
                SOURCE,
                "commit1",
                &[entry("a.json", None), entry("b.json", None)],
            )
            .unwrap();

        assert_eq!(again, id);
        let stored = db.snapshot_files(id).unwrap();
        assert_eq!(stored.len(), 2);
        assert!(stored[0].fetched);
        assert_eq!(stored[0].sha256, Some(digest));
        assert!(!stored[1].fetched);
    }

    #[test]
    fn recording_a_blob_for_an_unknown_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(dir.path());
        let id = db.record_snapshot(SOURCE, "commit1", &[]).unwrap();
        let key = FileKey {
            path: "missing",
            file_id: None,
        };

        let result = db.record_blob(id, key, &Sha256::of(b"abc"), 3, || Ok(()));

        assert!(matches!(result, Err(Error::Invalid(_))));
    }

    #[test]
    fn refs_are_replaced_and_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(dir.path());
        db.set_ref(SOURCE, "main", "commit1").unwrap();
        db.set_ref(SOURCE, "main", "commit2").unwrap();
        db.set_ref(SOURCE, "dev", "commit1").unwrap();
        drop(db);

        let db = open(dir.path());
        assert_eq!(
            db.refs(SOURCE).unwrap(),
            [
                ("dev".to_owned(), "commit1".to_owned()),
                ("main".to_owned(), "commit2".to_owned())
            ]
        );
        assert_eq!(db.refs("hf:huggingface.co:other").unwrap(), []);
    }

    #[test]
    fn a_database_from_a_newer_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        drop(open(dir.path()));
        let conn = Connection::open(dir.path().join("mvault.db")).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);

        let result = Db::open(&dir.path().join("mvault.db"));

        assert!(matches!(result, Err(Error::Invalid(_))));
    }
}
