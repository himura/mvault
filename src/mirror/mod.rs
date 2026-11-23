//! The mirror: a local copy of upstream models, served to nodes as static
//! files.
//!
//! ```text
//! <root>/public/    blobs/, snapshots/, metadata/   (what nodes read)
//! <root>/private/   mvault.db, tmp/                 (never served)
//! ```

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::download::{Client, Progress, Request};
use crate::error::{Error, Result};
use crate::fsutil::{io_error, write_atomic};
use crate::hash::Sha256;
use crate::store::BlobStore;
use crate::upstream::civitai::Site;
use crate::upstream::encode_segment;
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
    /// Files of the snapshot that this pull left out.
    pub skipped: usize,
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
    /// Opens the mirror at `root`, creating what is missing.
    ///
    /// # Errors
    ///
    /// Returns an error if the directories or the database cannot be
    /// created or opened.
    pub fn open(root: &Path) -> Result<Self> {
        let public = root.join("public");
        let private = root.join("private");
        for dir in [&public, &private] {
            fs::create_dir_all(dir).map_err(io_error(dir))?;
        }
        Ok(Self {
            db: Db::open(&private.join("mvault.db"))?,
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
        let source = format!("hf:{host}:{repo}");
        let info = hub.repo_info(http, repo, revision)?;
        let commit = info.commit;
        let dir = Path::new("hf").join(host_dir(host)).join(repo);
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
            .filter(|file| !excludes.matches(&file.path))
            .map(|file| Job {
                file,
                url: hub.file_url(repo, &commit, &file.path),
            })
            .collect();
        let mut pulled = Pulled::new(source, commit, files.len() - jobs.len());
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
                    self.db.set_ref(&pulled.source, revision, &pulled.revision)
                }
            });

        // Also after a failure: what was fetched before it is worth serving.
        let snapshots = self.public.join("snapshots").join(&dir);
        self.export_snapshot(snapshot_id, &pulled, &snapshots)?;
        for (name, target) in self.db.refs(&pulled.source)? {
            write_atomic(
                &snapshots.join("refs").join(encode_segment(&name)),
                format!("{target}\n").as_bytes(),
            )?;
        }
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
        let source = format!("civitai:{host}:{model_id}");
        let (version_id, latest_model) = if let Some(version_id) = version_id {
            (version_id, None)
        } else {
            let (latest, raw) = site.model(http, model_id)?;
            let latest =
                latest.ok_or_else(|| Error::Invalid(format!("{source} has no versions")))?;
            (latest, Some(raw))
        };
        let version = site.version(http, version_id)?;
        if version.model_id != model_id {
            return Err(Error::Invalid(format!(
                "version {version_id} belongs to model {}, not {model_id}",
                version.model_id
            )));
        }

        let dir = Path::new("civitai")
            .join(host_dir(host))
            .join(model_id.to_string());
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
        let mut pulled = Pulled::new(source, revision, files.len() - jobs.len());
        let result = self
            .fetch_all(http, snapshot_id, &jobs, &mut pulled, on_event, &|url| {
                site.headers_for(url)
            })
            .and_then(|()| {
                if resolved_latest {
                    self.db.set_ref(&pulled.source, "latest", &pulled.revision)
                } else {
                    Ok(())
                }
            });

        let snapshots = self.public.join("snapshots").join(&dir);
        self.export_snapshot(snapshot_id, &pulled, &snapshots)?;
        for (name, target) in self.db.refs(&pulled.source)? {
            write_atomic(
                &snapshots.join(encode_segment(&name)),
                format!("{target}\n").as_bytes(),
            )?;
        }
        result.map(|()| pulled)
    }

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
        for (index, job) in jobs.iter().enumerate() {
            on_event(Event::Started {
                index: index + 1,
                count: jobs.len(),
                path: &job.file.path,
            });
            let outcome = if job.file.fetched {
                Outcome::Present
            } else {
                let headers = headers_for(&job.url);
                self.fetch(http, snapshot_id, &identity, job, &headers, on_event)?
            };
            match outcome {
                Outcome::Downloaded => pulled.downloaded += 1,
                Outcome::Reused => pulled.reused += 1,
                Outcome::Present => pulled.present += 1,
            }
            on_event(Event::Finished(outcome));
        }
        Ok(())
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
            return Ok(Outcome::Reused);
        }

        let partial = self
            .store
            .partial_path(&partial_key(identity, file))
            .map_err(Error::Store)?;
        // One process at a time works on a partial file. The lock goes away
        // with the process, so a crash leaves nothing to clean up by hand.
        let lock_path = partial.with_extension("lock");
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(io_error(&lock_path))?;
        lock.lock().map_err(io_error(&lock_path))?;
        if let Some(digest) = &announced
            && self.db.use_stored_blob(snapshot_id, file.key(), digest)?
        {
            // Another process fetched it while this one waited for the lock.
            return Ok(Outcome::Reused);
        }

        if announced.is_none() {
            // Without a digest to compare with, the beginning left by an
            // earlier run cannot be told from that of a replaced file.
            remove_if_exists(&partial)?;
        }
        let request = Request {
            url: &job.url,
            headers,
            // Announced sizes are compared afterwards instead: one that is
            // wrong must fail the pull, not keep the download waiting.
            size: None,
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
            remove_if_exists(&partial)?;
            return Err(Error::Upstream {
                url: job.url.clone(),
                reason,
            });
        }

        // Flushing a large file takes long; it is done before the database is
        // locked, so that other pulls are not kept waiting for it.
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

    /// Writes the snapshot file nodes resolve a model from.
    fn export_snapshot(&self, snapshot_id: i64, pulled: &Pulled, dir: &Path) -> Result<()> {
        let files = self.db.snapshot_files(snapshot_id)?;
        let document = SnapshotDocument {
            schema: 1,
            source: &pulled.source,
            revision: &pulled.revision,
            files: files
                .iter()
                .map(|file| FileDocument {
                    path: &file.path,
                    file_id: file.file_id,
                    size: file.size,
                    sha256: file.sha256.map(|digest| digest.to_string()),
                    kind: "file",
                    primary: file.primary.then_some(true),
                    fetched: (!file.fetched).then_some(false),
                })
                .collect(),
        };
        let mut json = serde_json::to_vec_pretty(&document)
            .map_err(|e| Error::Invalid(format!("cannot write snapshot: {e}")))?;
        json.push(b'\n');
        write_atomic(&dir.join(format!("{}.json", pulled.revision)), &json)
    }
}

impl Pulled {
    fn new(source: String, revision: String, skipped: usize) -> Self {
        Self {
            source,
            revision,
            downloaded: 0,
            reused: 0,
            present: 0,
            skipped,
        }
    }
}

/// The snapshot file of the public tree. Optional fields are left out when
/// they say nothing: `primary` unless true, `fetched` unless false.
#[derive(Serialize)]
struct SnapshotDocument<'a> {
    schema: u32,
    source: &'a str,
    revision: &'a str,
    files: Vec<FileDocument<'a>>,
}

#[derive(Serialize)]
struct FileDocument<'a> {
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fetched: Option<bool>,
}

/// The directory name for a host: a port is separated by `_`, which no host
/// name contains, because `:` is not allowed in file names everywhere.
fn host_dir(host: &str) -> String {
    host.replace(':', "_")
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
