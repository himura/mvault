//! The mirror: a local copy of upstream models, served to nodes as static
//! files.
//!
//! ```text
//! <root>/public/    blobs/, snapshots/, metadata/   (what nodes read)
//! <root>/private/   mvault.db, tmp/                 (never served)
//! ```

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use log::debug;

use crate::download::{Client, Progress, Request};
use crate::error::{Error, Result};
use crate::fsutil::{io_error, lock_file, write_atomic};
use crate::gc::{PARTIAL_MAX_AGE, blob_at, clean_tmp};
use crate::hash::Sha256;
use crate::snapshot::{self, Snapshot, SnapshotFile};
use crate::source::{Defaults, Reference, Source};
use crate::store::BlobStore;
use crate::upstream::civitai::Site;
use crate::upstream::hf::Hub;

pub mod config;
pub mod db;
pub mod exclude;

use db::{Db, FileEntry};
use exclude::Excludes;

/// A mirror on the local filesystem.
#[derive(Debug)]
pub struct Mirror {
    public: PathBuf,
    db: Db,
    store: BlobStore,
}

/// Which files of a model version to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// The model itself.
    Primary,
    /// The files with these ids.
    Files(Vec<u64>),
    /// Every file.
    All,
}

/// What happened to a file during a pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The content was downloaded.
    Downloaded,
    /// The content was already stored as the blob of another file.
    Reused,
    /// The file had been fetched by an earlier pull.
    Present,
}

/// Something to tell the user while a pull runs.
#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    /// Work on a file begins. `index` counts from 1.
    Started {
        /// Position of the file among those to fetch.
        index: usize,
        /// Number of files to fetch.
        count: usize,
        /// Path or name of the file.
        path: &'a str,
    },
    /// A download has advanced.
    Progress(Progress),
    /// Work on the file last started is done.
    Finished(Outcome),
    /// Work on the file last started failed; the pull goes on with the
    /// next one.
    Failed(&'a str),
    /// Something went wrong that does not stop the pull.
    Warning(&'a str),
}

/// The result of a pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pulled {
    /// The source, in canonical form without a revision.
    pub source: String,
    /// The commit or version that was pulled.
    pub revision: String,
    /// Files whose content was downloaded.
    pub downloaded: usize,
    /// Files whose content was already stored under another name.
    pub reused: usize,
    /// Files an earlier pull had fetched.
    pub present: usize,
    /// Files of the snapshot that are not fetched after this pull: left
    /// out, or failed.
    pub skipped: usize,
}

/// A blob the database lists whose file is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingBlob {
    /// Digest of the blob.
    pub sha256: Sha256,
    /// The snapshots that had it as the content of a file, each as
    /// `<source>@<revision>`. Pulling them again restores the blob.
    pub snapshots: Vec<String>,
}

/// What a garbage collection removed, or would remove, and what it found
/// out of order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collected {
    /// Blobs no snapshot uses, with their sizes.
    pub blobs: Vec<(Sha256, u64)>,
    /// Partial files of downloads given up long ago.
    pub partials: Vec<PathBuf>,
    /// Blobs the database listed whose files are gone. Unless this was a
    /// dry run, they are forgotten and the files that had them as content
    /// are no longer marked as fetched.
    pub missing: Vec<MissingBlob>,
    /// Files among the blobs that the database does not list. They are
    /// left alone: a pull that is storing a blob right now looks like this.
    pub untracked: Vec<PathBuf>,
}

/// What to pull from a hub.
#[derive(Debug, Clone, Copy)]
pub struct HfPull<'a> {
    /// The hub to ask.
    pub hub: &'a Hub,
    /// Host of the hub, as in the canonical form of the source.
    pub host: &'a str,
    /// Repository id.
    pub repo: &'a str,
    /// Branch, tag or commit.
    pub revision: &'a str,
    /// Files to leave out.
    pub excludes: &'a Excludes,
}

/// What to pull from a site.
#[derive(Debug, Clone, Copy)]
pub struct CivitaiPull<'a> {
    /// The site to ask.
    pub site: &'a Site,
    /// Host of the site, as in the canonical form of the source.
    pub host: &'a str,
    /// Id of the model.
    pub model_id: u64,
    /// Id of the version; `None` stands for the latest one.
    pub version_id: Option<u64>,
    /// Which files of the version to fetch.
    pub selection: &'a Selection,
}

/// One file to fetch.
struct Job<'a> {
    file: &'a FileEntry,
    url: String,
}

impl Mirror {
    /// Makes `root` a mirror, or opens the mirror that is there. Returns
    /// whether it was created.
    ///
    /// # Errors
    ///
    /// Returns an error if the directories or the database cannot be
    /// created or opened.
    pub fn create(root: &Path) -> Result<(Self, bool)> {
        let created = !db_path(root).is_file();
        for dir in [root.join("public"), root.join("private")] {
            fs::create_dir_all(&dir).map_err(io_error(&dir))?;
        }
        Ok((Self::at(root)?, created))
    }

    /// Opens the mirror at `root`. Nothing is created: a mistyped root is
    /// an error, not a new, empty mirror.
    ///
    /// # Errors
    ///
    /// Returns an error if there is no mirror at `root`, or its database
    /// cannot be opened.
    pub fn open(root: &Path) -> Result<Self> {
        if !db_path(root).is_file() {
            return Err(Error::Invalid(format!(
                "{} is not a mirror (no private/mvault.db); mvault mirror init creates one",
                root.display()
            )));
        }
        Self::at(root)
    }

    fn at(root: &Path) -> Result<Self> {
        let public = root.join("public");
        let private = root.join("private");
        Ok(Self {
            db: Db::open(&db_path(root))?,
            store: BlobStore::new(public.join("blobs"), private.join("tmp")),
            public,
        })
    }

    /// The directory nodes read from.
    #[must_use]
    pub fn public_dir(&self) -> &Path {
        &self.public
    }

    /// Fetches the files of a repository at `revision`, except those
    /// `excludes` matches.
    ///
    /// The snapshot written for nodes lists every file of the repository;
    /// those not fetched are marked as such. Pulling the same commit again
    /// only adds to what is fetched. `revision`, if it is a name, stands for
    /// the commit once every file asked for is fetched, and not before.
    ///
    /// # Errors
    ///
    /// Returns an error if the hub cannot be asked, a download fails, or
    /// content does not have the digest or size the hub announced. Files
    /// fetched before the error stay fetched.
    pub fn pull_hf(
        &mut self,
        http: &Client,
        pull: &HfPull<'_>,
        on_event: &mut dyn FnMut(Event<'_>),
    ) -> Result<Pulled> {
        let HfPull {
            hub,
            host,
            repo,
            revision,
            excludes,
        } = *pull;
        let origin = Source::HuggingFace {
            host: host.to_owned(),
            repo: repo.to_owned(),
        };
        let source = origin.to_string();
        let info = hub.repo_info(http, repo, revision)?;
        let commit = info.commit;
        debug!(
            "{source}: revision {revision} is commit {commit}, with {} file(s)",
            info.files.len()
        );
        let dir = snapshot::source_dir(&origin);
        write_atomic(
            &self
                .public
                .join("metadata")
                .join(&dir)
                .join(format!("{commit}.json")),
            &info.raw,
        )?;

        let listed: Vec<FileEntry> = info
            .files
            .into_iter()
            .map(|file| FileEntry {
                path: file.path,
                file_id: None,
                size: file.size,
                sha256: file.sha256,
                primary: false,
                fetched: false,
            })
            .collect();
        let snapshot_id = self.db.record_snapshot(&source, &commit, &listed)?;

        let files = self.db.snapshot_files(snapshot_id)?;
        let jobs: Vec<Job<'_>> = files
            .iter()
            .filter(|file| {
                let excluded = excludes.matches(&file.path);
                if excluded {
                    debug!("{}: excluded, not fetched", file.path);
                }
                !excluded
            })
            .map(|file| Job {
                file,
                url: hub.file_url(repo, &commit, &file.path),
            })
            .collect();
        let mut pulled = Pulled::new(source, commit);
        let result = self
            .fetch_all(http, snapshot_id, &jobs, &mut pulled, on_event, &|url| {
                hub.headers_for(url)
            })
            // A name only moves to a snapshot that has what was asked for:
            // after a failure it keeps standing for the one nodes can use.
            .and_then(|()| {
                if revision == pulled.revision {
                    Ok(())
                } else {
                    debug!(
                        "{}: {revision} now stands for {}",
                        pulled.source, pulled.revision
                    );
                    self.db.set_ref(&pulled.source, revision, &pulled.revision)
                }
            });

        // Also after a failure: what was fetched is worth serving.
        self.export(snapshot_id, &origin, &pulled.revision)?;
        pulled.skipped = self.not_fetched(snapshot_id)?;
        result.map(|()| pulled)
    }

    /// Fetches files of a model version; of the latest one if `version_id`
    /// is `None`, which then becomes what "latest" stands for once the files
    /// asked for are fetched.
    ///
    /// # Errors
    ///
    /// Returns an error if the site cannot be asked, the selection does not
    /// fit the version, a download fails, or content does not have the
    /// digest the site announced. Files fetched before the error stay
    /// fetched.
    pub fn pull_civitai(
        &mut self,
        http: &Client,
        pull: &CivitaiPull<'_>,
        on_event: &mut dyn FnMut(Event<'_>),
    ) -> Result<Pulled> {
        let CivitaiPull {
            site,
            host,
            model_id,
            version_id,
            selection,
        } = *pull;
        let origin = Source::Civitai {
            host: host.to_owned(),
            model_id,
            version_id: None,
        };
        let source = origin.to_string();
        let (version_id, latest_model) = if let Some(version_id) = version_id {
            (version_id, None)
        } else {
            let (latest, raw) = site.model(http, model_id)?;
            let latest =
                latest.ok_or_else(|| Error::Invalid(format!("{source} has no versions")))?;
            debug!("{source}: the latest version is {latest}");
            (latest, Some(raw))
        };
        let version = site.version(http, version_id)?;
        if version.model_id != model_id {
            return Err(Error::Invalid(format!(
                "version {version_id} belongs to model {}, not {model_id}",
                version.model_id
            )));
        }

        let dir = snapshot::source_dir(&origin);
        let metadata = self.public.join("metadata").join(&dir);
        write_atomic(&metadata.join(format!("{version_id}.json")), &version.raw)?;
        let resolved_latest = latest_model.is_some();
        // The description of the model is kept for when the site is gone; the
        // pull does not depend on it.
        let model = match latest_model {
            Some(raw) => Some(raw),
            None => match site.model(http, model_id) {
                Ok((_, raw)) => Some(raw),
                Err(error) => {
                    on_event(Event::Warning(&format!(
                        "could not fetch the description of model {model_id}: {error}"
                    )));
                    None
                }
            },
        };
        if let Some(raw) = model {
            write_atomic(&metadata.join("model.json"), &raw)?;
        }

        let listed: Vec<FileEntry> = version
            .files
            .iter()
            .map(|file| FileEntry {
                path: file.name.clone(),
                file_id: Some(file.id),
                size: None,
                sha256: file.sha256,
                primary: file.primary,
                fetched: false,
            })
            .collect();
        let wanted = select(&listed, selection, version_id)?;
        log_selection(&listed, &wanted, selection);

        let revision = version_id.to_string();
        let snapshot_id = self.db.record_snapshot(&source, &revision, &listed)?;

        let files = self.db.snapshot_files(snapshot_id)?;
        let jobs: Vec<Job<'_>> = files
            .iter()
            .filter(|file| file.file_id.is_some_and(|id| wanted.contains(&id)))
            .filter_map(|file| {
                let upstream = version
                    .files
                    .iter()
                    .find(|other| Some(other.id) == file.file_id)?;
                Some(Job {
                    file,
                    url: upstream.download_url.clone(),
                })
            })
            .collect();
        let mut pulled = Pulled::new(source, revision);
        let result = self
            .fetch_all(http, snapshot_id, &jobs, &mut pulled, on_event, &|url| {
                site.headers_for(url)
            })
            .and_then(|()| {
                if resolved_latest {
                    debug!(
                        "{}: latest now stands for {}",
                        pulled.source, pulled.revision
                    );
                    self.db.set_ref(&pulled.source, "latest", &pulled.revision)
                } else {
                    Ok(())
                }
            });

        self.export(snapshot_id, &origin, &pulled.revision)?;
        pulled.skipped = self.not_fetched(snapshot_id)?;
        result.map(|()| pulled)
    }

    fn not_fetched(&self, snapshot_id: i64) -> Result<usize> {
        Ok(self
            .db
            .snapshot_files(snapshot_id)?
            .iter()
            .filter(|file| !file.fetched)
            .count())
    }

    /// Fetches the files of `jobs`. A file that fails does not stop the
    /// others; the failures are reported together at the end.
    fn fetch_all<'h>(
        &mut self,
        http: &Client,
        snapshot_id: i64,
        jobs: &[Job<'_>],
        pulled: &mut Pulled,
        on_event: &mut dyn FnMut(Event<'_>),
        headers_for: &dyn Fn(&str) -> Vec<(&'h str, &'h str)>,
    ) -> Result<()> {
        let identity = format!("{}@{}", pulled.source, pulled.revision);
        let mut failures = Vec::new();
        for (index, job) in jobs.iter().enumerate() {
            on_event(Event::Started {
                index: index + 1,
                count: jobs.len(),
                path: &job.file.path,
            });
            let outcome = if job.file.fetched {
                debug!("{}: fetched by an earlier pull", job.file.path);
                Outcome::Present
            } else {
                let headers = headers_for(&job.url);
                match self.fetch(http, snapshot_id, &identity, job, &headers, on_event) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let message = error.to_string();
                        on_event(Event::Failed(&message));
                        failures.push(format!("{}: {message}", job.file.path));
                        continue;
                    }
                }
            };
            match outcome {
                Outcome::Downloaded => pulled.downloaded += 1,
                Outcome::Reused => pulled.reused += 1,
                Outcome::Present => pulled.present += 1,
            }
            on_event(Event::Finished(outcome));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error::Invalid(format!(
                "{} of {} file(s) could not be fetched:\n  {}",
                failures.len(),
                jobs.len(),
                failures.join("\n  ")
            )))
        }
    }

    /// Makes the content of one file a blob of the mirror.
    fn fetch(
        &mut self,
        http: &Client,
        snapshot_id: i64,
        identity: &str,
        job: &Job<'_>,
        headers: &[(&str, &str)],
        on_event: &mut dyn FnMut(Event<'_>),
    ) -> Result<Outcome> {
        let file = job.file;
        let announced = file.sha256;
        if let Some(digest) = &announced
            && self.db.use_stored_blob(snapshot_id, file.key(), digest)?
        {
            debug!("{}: blob {digest} is stored already", file.path);
            return Ok(Outcome::Reused);
        }

        let partial = self
            .store
            .partial_path(&partial_key(identity, file))
            .map_err(Error::Store)?;
        // One process at a time works on a partial file. The lock goes away
        // with the process, so a crash leaves nothing to clean up by hand.
        let lock_path = partial.with_extension("lock");
        let _guard = lock_file(&lock_path)?;
        if let Some(digest) = &announced
            && self.db.use_stored_blob(snapshot_id, file.key(), digest)?
        {
            // Another process fetched it while this one waited for the lock.
            debug!(
                "{}: blob {digest} was stored by another process meanwhile",
                file.path
            );
            return Ok(Outcome::Reused);
        }
        if self.db.is_fetched(snapshot_id, file.key())? {
            // The same, for a file whose digest was not announced.
            debug!("{}: fetched by another process meanwhile", file.path);
            return Ok(Outcome::Present);
        }

        if announced.is_none() {
            // Without a digest to compare with, the beginning left by an
            // earlier run cannot be told from that of a replaced file.
            debug!(
                "{}: no digest announced, so a partial file is not continued",
                file.path
            );
            remove_if_exists(&partial)?;
        }
        let request = Request {
            url: &job.url,
            headers,
            // Nothing past the announced size is written, and a server that
            // announces another size fails before anything is read. A
            // partial file of the announced size is complete already.
            size: file.size,
        };
        let downloaded = http.download(&request, &partial, &mut |progress| {
            on_event(Event::Progress(progress));
        })?;

        let mismatch = match (announced, file.size) {
            (Some(digest), _) if digest != downloaded.sha256 => Some(format!(
                "expected sha256 {digest}, got {}",
                downloaded.sha256
            )),
            (_, Some(size)) if size != downloaded.size => {
                Some(format!("expected {size} bytes, got {}", downloaded.size))
            }
            _ => None,
        };
        if let Some(reason) = mismatch {
            debug!("{}: {reason}; removing {}", file.path, partial.display());
            remove_if_exists(&partial)?;
            return Err(Error::Upstream {
                url: job.url.clone(),
                reason,
            });
        }

        // Flushing a large file takes long; it is done before the database is
        // locked, so that other pulls are not kept waiting for it.
        debug!(
            "{}: storing {} as blob {}",
            file.path,
            partial.display(),
            downloaded.sha256
        );
        let prepared = self
            .store
            .prepare(&partial, &downloaded.sha256)
            .map_err(Error::Store)?;
        self.db.record_blob(
            snapshot_id,
            file.key(),
            &downloaded.sha256,
            downloaded.size,
            || prepared.commit().map(|_| ()).map_err(Error::Store),
        )?;
        Ok(Outcome::Downloaded)
    }

    /// Removes the blobs no snapshot uses that have been stored for at
    /// least `min_age`, and what downloads left behind. With `dry_run`,
    /// only reports.
    ///
    /// A blob is kept for `min_age` because a pull stores it before the
    /// snapshot that uses it is complete.
    ///
    /// # Errors
    ///
    /// Returns an error if the database or the files cannot be read or
    /// changed.
    pub fn gc(&mut self, min_age: Duration, dry_run: bool) -> Result<Collected> {
        let cutoff = db::now().saturating_sub(min_age.as_secs());
        let store = &self.store;
        // Blobs that cannot be seen are not gone: without their directory,
        // every blob would be forgotten and downloaded again.
        if !self.db.blob_digests()?.is_empty() {
            store.check_present().map_err(Error::Store)?;
        }
        let blobs = if dry_run {
            self.db.unused_blobs(cutoff)?
        } else {
            self.db
                .remove_unused_blobs(cutoff, |digest| store.remove(digest).map_err(Error::Store))?
        };
        let partials = clean_tmp(store.tmp_dir(), PARTIAL_MAX_AGE, dry_run)?;

        // The database and the directory should agree on what is stored.
        // A blob the database lists without a file is forgotten, so that
        // a pull downloads it again instead of taking it as stored.
        let listed = self.db.blob_digests()?;
        let mut missing = Vec::new();
        for digest in &listed {
            // In a dry run the unused blobs are still listed; whether
            // their files exist no longer matters.
            let unused = blobs.iter().any(|(blob, _)| blob == digest);
            if !unused && !store.find(digest).map_err(Error::Store)? {
                debug!("blob {digest} is in the database, but its file is gone");
                let snapshots = self
                    .db
                    .blob_snapshots(digest)?
                    .into_iter()
                    .map(|(source, revision)| format!("{source}@{revision}"))
                    .collect();
                missing.push(MissingBlob {
                    sha256: *digest,
                    snapshots,
                });
            }
        }
        if !dry_run {
            let gone: Vec<Sha256> = missing.iter().map(|blob| blob.sha256).collect();
            let forgotten = self.db.forget_blobs(&gone, |digest| {
                store.find(digest).map(|found| !found).map_err(Error::Store)
            })?;
            // A pull stored the blob again in the meantime.
            missing.retain(|blob| forgotten.contains(&blob.sha256));
            // Nodes read from the snapshot files which files are fetched.
            let changed: std::collections::BTreeSet<&str> = missing
                .iter()
                .flat_map(|blob| blob.snapshots.iter().map(String::as_str))
                .collect();
            for (id, source, revision) in self.db.snapshots()? {
                if changed.contains(format!("{source}@{revision}").as_str()) {
                    self.export(id, &parse_source(&source)?, &revision)?;
                }
            }
        }
        let known = self.db.blob_digests()?;
        let store = &self.store;
        let untracked = store
            .blob_files()
            .map_err(Error::Store)?
            .into_iter()
            .filter(|path| {
                // Only a file at the place of its digest is that blob.
                blob_at(store, path).is_none_or(|digest| known.binary_search(&digest).is_err())
            })
            .collect();
        Ok(Collected {
            blobs,
            partials,
            missing,
            untracked,
        })
    }

    /// Writes the files of every snapshot for nodes again, from the
    /// database. Returns how many snapshots were written.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be read or a file cannot be
    /// written.
    pub fn export_all(&self) -> Result<usize> {
        let snapshots = self.db.snapshots()?;
        for (id, source, revision) in &snapshots {
            self.export(*id, &parse_source(source)?, revision)?;
        }
        Ok(snapshots.len())
    }

    /// Writes the files nodes resolve a model from: the snapshot, and the
    /// names that stand for revisions of the source. Every command that
    /// changes what a snapshot has fetched calls this when it is done.
    fn export(&self, snapshot_id: i64, origin: &Source, revision: &str) -> Result<()> {
        let source = origin.to_string();
        let files = self.db.snapshot_files(snapshot_id)?;
        let document = Snapshot {
            schema: snapshot::SCHEMA,
            source: source.clone(),
            revision: revision.to_owned(),
            files: files
                .into_iter()
                .map(|file| SnapshotFile {
                    path: file.path,
                    file_id: file.file_id,
                    size: file.size,
                    sha256: file.sha256,
                    kind: "file".to_owned(),
                    primary: file.primary,
                    fetched: file.fetched,
                })
                .collect(),
        };
        let mut json = serde_json::to_vec_pretty(&document)
            .map_err(|e| Error::Invalid(format!("cannot write snapshot: {e}")))?;
        json.push(b'\n');
        write_atomic(
            &self.public.join(snapshot::snapshot_path(origin, revision)),
            &json,
        )?;
        for (name, target) in self.db.refs(&source)? {
            write_atomic(
                &self.public.join(snapshot::ref_path(origin, &name)),
                format!("{target}\n").as_bytes(),
            )?;
        }
        Ok(())
    }
}

impl Pulled {
    fn new(source: String, revision: String) -> Self {
        Self {
            source,
            revision,
            downloaded: 0,
            reused: 0,
            present: 0,
            skipped: 0,
        }
    }
}

fn db_path(root: &Path) -> PathBuf {
    root.join("private").join("mvault.db")
}

/// The source a snapshot of the database names.
fn parse_source(source: &str) -> Result<Source> {
    Reference::parse(source, &Defaults::default())
        .map(|reference| reference.source)
        .map_err(|e| Error::Invalid(format!("database: {e}")))
}

/// The name of the partial file for a download: the digest if it is known,
/// so that the same content from two sources is downloaded once, and
/// otherwise a digest of what identifies the file.
fn partial_key(identity: &str, file: &FileEntry) -> String {
    if let Some(digest) = file.sha256 {
        return digest.to_string();
    }
    let name = format!("{identity}\0{}\0{}", file.path, file.file_id.unwrap_or(0));
    Sha256::of(name.as_bytes()).to_string()
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(io_error(path)(error)),
        _ => Ok(()),
    }
}

/// The ids of the files of a version that `selection` asks for.
fn select(listed: &[FileEntry], selection: &Selection, version_id: u64) -> Result<Vec<u64>> {
    match selection {
        Selection::All => Ok(listed.iter().filter_map(|file| file.file_id).collect()),
        Selection::Primary => {
            let primary = listed.iter().find(|file| file.primary).ok_or_else(|| {
                Error::Invalid(format!(
                    "version {version_id} has no single primary file; choose by file id ({})",
                    describe(listed)
                ))
            })?;
            Ok(primary.file_id.into_iter().collect())
        }
        Selection::Files(ids) => {
            for id in ids {
                if !listed.iter().any(|file| file.file_id == Some(*id)) {
                    return Err(Error::Invalid(format!(
                        "version {version_id} has no file {id} ({})",
                        describe(listed)
                    )));
                }
            }
            Ok(ids.clone())
        }
    }
}

/// Tells which files of a version `selection` chose, and which it left out.
fn log_selection(listed: &[FileEntry], wanted: &[u64], selection: &Selection) {
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }
    let asked = match selection {
        Selection::Primary => "the primary file".to_owned(),
        Selection::All => "every file".to_owned(),
        Selection::Files(ids) => format!(
            "file id {}",
            ids.iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    debug!("selected {asked} of {} file(s)", listed.len());
    for file in listed {
        if file.file_id.is_some_and(|id| !wanted.contains(&id)) {
            debug!("{}: not selected, not fetched", file.path);
        }
    }
}

/// The files of a version, for an error message.
fn describe(files: &[FileEntry]) -> String {
    files
        .iter()
        .map(|file| format!("{}: {}", file.file_id.unwrap_or(0), file.path))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests;
