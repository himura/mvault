//! Making the workspace match the manifest: resolve what is new, fetch the
//! blobs this node lacks, and hard-link them into place.
//!
//! A placed file is a hard link to a blob, and that is also how mvault knows
//! its own files: a file that is the same file as a blob is managed, any
//! other file belongs to the user and is left alone.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use super::lock::Lock;
use super::manifest::{Manifest, STATE_DIR};
use super::plan::{Placement, plan, unresolved};
use super::resolve::{Access, fill_digests, parse_source, resolve};
use super::workspace::Workspace;
use crate::download::{Downloaded, Progress, Request};
use crate::error::{Error, Result};
use crate::fsutil::io_error;
use crate::hash::Sha256;
use crate::source::Source;
use crate::store::BlobStore;

/// How to sync.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Options {
    /// Only report what would be done.
    pub dry_run: bool,
    /// Fail rather than resolve models the lock file does not have.
    pub locked: bool,
    /// Remove managed files the manifest no longer places.
    pub prune: bool,
    /// Replace files that are in the way.
    pub force: bool,
}

/// What a sync did, or with `dry_run` would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Models resolved and added to the lock file.
    pub resolved: usize,
    /// Blobs fetched.
    pub fetched: usize,
    /// Files linked into place.
    pub linked: usize,
    /// Files that were already in place.
    pub unchanged: usize,
    /// Managed files removed.
    pub pruned: usize,
    /// Destinations taken by files mvault does not manage.
    pub conflicts: Vec<String>,
}

/// Something to tell the user while a sync runs.
#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    /// A model was resolved.
    Resolved {
        /// The source of the model.
        source: &'a str,
        /// The commit or version it stands for.
        resolved: &'a str,
    },
    /// A blob is about to be fetched. `index` counts from 1.
    Fetching {
        /// Position among the blobs to fetch.
        index: usize,
        /// Number of blobs to fetch.
        count: usize,
        /// Destination of the file that needs it.
        dest: &'a str,
    },
    /// The fetch has advanced.
    Progress(Progress),
    /// The blob last announced is stored.
    Fetched,
    /// A file was linked into place.
    Linked(&'a str),
    /// A destination is taken by a file mvault does not manage.
    Conflict(&'a str),
    /// A managed file the manifest no longer places was removed.
    Pruned(&'a str),
}

/// Makes `workspace` match its manifest.
///
/// # Errors
///
/// Returns an error if the manifest or lock file is not valid, a model
/// cannot be resolved, or a blob cannot be fetched or does not have the
/// digest of the lock file. Destinations taken by other files are not an
/// error here; they are listed in the report.
pub fn sync(
    workspace: &Workspace,
    access: &Access<'_>,
    options: Options,
    on_event: &mut dyn FnMut(Event<'_>),
) -> Result<Report> {
    let mut report = Report::default();
    let manifest = Manifest::load(&workspace.root, &workspace.defaults())?;
    let lock_path = workspace.lock_path();
    let mut lock = Lock::load(&lock_path)?;
    let lock_changed =
        bring_up_to_date(&manifest, &mut lock, access, options, &mut report, on_event)?;
    if lock_changed && !options.dry_run {
        lock.save(&lock_path)?;
    }

    let mut placements = plan(&manifest, &lock)?;
    let mut seen = BTreeSet::new();
    let to_fetch: Vec<usize> = placements
        .iter()
        .enumerate()
        .filter(|(_, placement)| match placement.sha256 {
            Some(digest) => !workspace.store.contains(&digest) && seen.insert(digest),
            None => true,
        })
        .map(|(index, _)| index)
        .collect();
    report.fetched = to_fetch.len();
    if !options.dry_run {
        report.fetched = 0;
        for (position, index) in to_fetch.iter().enumerate() {
            if placements[*index].sha256.is_some() && is_stored(workspace, &placements[*index]) {
                // The same file, placed twice, was fetched for the other place.
                continue;
            }
            on_event(Event::Fetching {
                index: position + 1,
                count: to_fetch.len(),
                dest: &placements[*index].dest,
            });
            let digest = fetch(workspace, access, &placements[*index], on_event)?;
            on_event(Event::Fetched);
            report.fetched += 1;
            if placements[*index].sha256.is_none() {
                record_digest(&mut lock, &placements[*index], digest);
                lock.save(&lock_path)?;
                let fetched = placements[*index].clone();
                for placement in &mut placements {
                    if same_file(placement, &fetched) {
                        placement.sha256 = Some(digest);
                    }
                }
            }
        }
    }

    let mut managed = Managed::new(&workspace.store);
    for placement in &placements {
        let dest = workspace.root.join(&placement.dest);
        let blob = placement
            .sha256
            .map(|digest| workspace.store.blob_path(&digest));
        match state(&dest, blob.as_deref(), &mut managed)? {
            State::InPlace => report.unchanged += 1,
            State::Taken if !options.force => {
                on_event(Event::Conflict(&placement.dest));
                report.conflicts.push(placement.dest.clone());
            }
            State::Free | State::Managed | State::Taken => {
                if let (false, Some(blob)) = (options.dry_run, &blob) {
                    link(blob, &dest)?;
                }
                on_event(Event::Linked(&placement.dest));
                report.linked += 1;
            }
        }
    }

    if options.prune {
        prune(
            workspace,
            &placements,
            &mut managed,
            options.dry_run,
            &mut report,
            on_event,
        )?;
    }
    Ok(report)
}

/// Resolves the models the lock lacks and fills in digests a mirror knows
/// by now. Returns whether the lock changed.
fn bring_up_to_date(
    manifest: &Manifest,
    lock: &mut Lock,
    access: &Access<'_>,
    options: Options,
    report: &mut Report,
    on_event: &mut dyn FnMut(Event<'_>),
) -> Result<bool> {
    let mut changed = false;
    for model in unresolved(manifest, lock) {
        if options.locked {
            return Err(Error::Invalid(format!(
                "{} is not in the lock file, and --locked forbids resolving it",
                model.source
            )));
        }
        let locked = resolve(model, access)?;
        on_event(Event::Resolved {
            source: &locked.source,
            resolved: &locked.resolved,
        });
        lock.insert(locked);
        report.resolved += 1;
        changed = true;
    }

    // Digests the upstream did not announce may be known to a mirror by now.
    let without_digest: BTreeSet<(String, Option<String>)> = plan(manifest, lock)?
        .into_iter()
        .filter(|placement| placement.sha256.is_none())
        .map(|placement| (placement.source, placement.revision))
        .collect();
    for (source, revision) in &without_digest {
        if let Some(locked) = lock.get_mut(source, revision.as_deref())
            && fill_digests(locked, access)?
        {
            changed = true;
        }
    }
    Ok(changed)
}

fn is_stored(workspace: &Workspace, placement: &Placement) -> bool {
    placement
        .sha256
        .is_some_and(|digest| workspace.store.contains(&digest))
}

/// Whether two placements are of the same file of the same model.
fn same_file(a: &Placement, b: &Placement) -> bool {
    (&a.source, &a.revision, &a.path, a.file_id) == (&b.source, &b.revision, &b.path, b.file_id)
}

/// Removes managed files that the manifest no longer places, from the
/// directories it places files in.
fn prune(
    workspace: &Workspace,
    placements: &[Placement],
    managed: &mut Managed<'_>,
    dry_run: bool,
    report: &mut Report,
    on_event: &mut dyn FnMut(Event<'_>),
) -> Result<()> {
    let wanted: BTreeSet<&str> = placements
        .iter()
        .map(|placement| placement.dest.as_str())
        .collect();
    for dir in top_dirs(&wanted) {
        let mut files = Vec::new();
        walk(&workspace.root, &dir, &mut files)?;
        for file in files {
            let path = workspace.root.join(&file);
            if !wanted.contains(file.as_str()) && managed.contains(&path)? {
                if !dry_run {
                    fs::remove_file(&path).map_err(io_error(&path))?;
                }
                on_event(Event::Pruned(&file));
                report.pruned += 1;
            }
        }
    }
    Ok(())
}

/// Writes the digest of a file fetched for the first time into the lock.
fn record_digest(lock: &mut Lock, placement: &Placement, digest: Sha256) {
    let locked = lock.get_mut(&placement.source, placement.revision.as_deref());
    let file = locked.and_then(|locked| {
        locked
            .files
            .iter_mut()
            .find(|file| file.path == placement.path && file.file_id == placement.file_id)
    });
    if let Some(file) = file {
        file.sha256 = Some(digest);
    }
}

/// Makes the content of a placement a blob of this node, from the first
/// mirror that has it, or from the upstream if the settings allow.
fn fetch(
    workspace: &Workspace,
    access: &Access<'_>,
    placement: &Placement,
    on_event: &mut dyn FnMut(Event<'_>),
) -> Result<Sha256> {
    let store = &workspace.store;
    let key = match placement.sha256 {
        Some(digest) => digest.to_string(),
        None => Sha256::of(
            format!(
                "{}\0{}\0{}\0{}",
                placement.source,
                placement.resolved,
                placement.path,
                placement.file_id.unwrap_or(0)
            )
            .as_bytes(),
        )
        .to_string(),
    };
    let partial = store.partial_path(&key).map_err(Error::Store)?;
    // One process at a time works on a partial file; another sync waits.
    let lock_path = partial.with_extension("lock");
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(io_error(&lock_path))?;
    lock.lock().map_err(io_error(&lock_path))?;

    if let Some(digest) = placement.sha256 {
        if store.contains(&digest) {
            return Ok(digest);
        }
        for remote in access.remotes {
            let fetched = remote.fetch_blob(access.http, &digest, &partial, &mut |progress| {
                on_event(Event::Progress(progress));
            });
            match fetched {
                Ok(Some(downloaded)) if downloaded.sha256 == digest => {
                    store.commit(&partial, &digest).map_err(Error::Store)?;
                    return Ok(digest);
                }
                Ok(Some(downloaded)) => {
                    remove_if_exists(&partial)?;
                    return Err(Error::Invalid(format!(
                        "mirror {remote}: blob {digest} has the content of {}",
                        downloaded.sha256
                    )));
                }
                Ok(None) => {}
                Err(error) => access.warn(format!("mirror {remote}: {error}")),
            }
        }
    }

    let source = parse_source(&placement.source)?;
    if !workspace.config.allow_official {
        return Err(Error::Invalid(format!(
            "{}: no mirror has {} of {}; on the mirror host run: {}",
            placement.dest,
            placement.path,
            placement.source,
            pull_command(&source, placement)
        )));
    }
    if placement.sha256.is_none() {
        // Without a digest to compare with, the beginning left by an
        // earlier run cannot be told from that of a replaced file.
        remove_if_exists(&partial)?;
    }
    let (url, downloaded) = from_upstream(access, &source, placement, &partial, on_event)?;
    let mismatch = match (placement.sha256, placement.size) {
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
        return Err(Error::Upstream { url, reason });
    }
    store
        .commit(&partial, &downloaded.sha256)
        .map_err(Error::Store)?;
    Ok(downloaded.sha256)
}

/// Downloads the file of a placement from where the model comes from.
/// Returns the URL it was downloaded from as well.
fn from_upstream(
    access: &Access<'_>,
    source: &Source,
    placement: &Placement,
    partial: &Path,
    on_event: &mut dyn FnMut(Event<'_>),
) -> Result<(String, Downloaded)> {
    let mut progress = |progress| on_event(Event::Progress(progress));
    match source {
        Source::HuggingFace { host, repo } => {
            let hub = (access.hub)(host);
            let url = hub.file_url(repo, &placement.resolved, &placement.path);
            let request = Request {
                url: &url,
                headers: &hub.headers_for(&url),
                size: None,
            };
            let downloaded = access.http.download(&request, partial, &mut progress)?;
            Ok((url, downloaded))
        }
        Source::Civitai { host, .. } => {
            let site = (access.site)(host);
            let version_id = placement.resolved.parse().map_err(|_| {
                Error::Invalid(format!(
                    "{}: {:?} is not a version id",
                    placement.source, placement.resolved
                ))
            })?;
            let version = site.version(access.http, version_id)?;
            let file = version
                .files
                .into_iter()
                .find(|file| Some(file.id) == placement.file_id)
                .ok_or_else(|| {
                    Error::Invalid(format!(
                        "{}: version {version_id} no longer has file {}",
                        placement.source,
                        placement.file_id.unwrap_or(0)
                    ))
                })?;
            let request = Request {
                url: &file.download_url,
                headers: &site.headers_for(&file.download_url),
                size: None,
            };
            let downloaded = access.http.download(&request, partial, &mut progress)?;
            Ok((file.download_url, downloaded))
        }
    }
}

/// The command that makes a mirror hold the file of a placement.
fn pull_command(source: &Source, placement: &Placement) -> String {
    match source {
        Source::HuggingFace { host, repo } => format!(
            "mvault mirror pull hf:{host}:{repo} --revision {}",
            placement.resolved
        ),
        Source::Civitai { host, model_id, .. } => {
            let file = placement
                .file_id
                .map_or_else(String::new, |id| format!(" --file-id {id}"));
            format!(
                "mvault mirror pull civitai:{host}:{model_id}@{}{file}",
                placement.resolved
            )
        }
    }
}

/// What is at a destination.
enum State {
    /// Nothing.
    Free,
    /// The blob it should be.
    InPlace,
    /// Another blob: a file mvault placed earlier.
    Managed,
    /// Something mvault does not manage.
    Taken,
}

fn state(dest: &Path, blob: Option<&Path>, managed: &mut Managed<'_>) -> Result<State> {
    let metadata = match fs::symlink_metadata(dest) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(State::Free),
        Err(error) => return Err(io_error(dest)(error)),
    };
    if !metadata.is_file() {
        return Ok(State::Taken);
    }
    if let Some(blob) = blob
        && blob.is_file()
        && same_file::is_same_file(dest, blob).map_err(io_error(dest))?
    {
        return Ok(State::InPlace);
    }
    Ok(if managed.contains(dest)? {
        State::Managed
    } else {
        State::Taken
    })
}

/// Tells whether a file is a hard link to one of the blobs. The blobs are
/// listed once, on first use, and compared by size before anything is
/// opened.
struct Managed<'a> {
    store: &'a BlobStore,
    by_size: Option<HashMap<u64, Vec<PathBuf>>>,
}

impl<'a> Managed<'a> {
    fn new(store: &'a BlobStore) -> Self {
        Self {
            store,
            by_size: None,
        }
    }

    fn contains(&mut self, path: &Path) -> Result<bool> {
        let by_size = match &mut self.by_size {
            Some(by_size) => by_size,
            none => {
                let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
                for blob in self.store.blob_files().map_err(Error::Store)? {
                    if let Ok(metadata) = fs::metadata(&blob) {
                        by_size.entry(metadata.len()).or_default().push(blob);
                    }
                }
                none.insert(by_size)
            }
        };
        let size = fs::metadata(path).map_err(io_error(path))?.len();
        let candidates = by_size.get(&size).map_or(&[][..], Vec::as_slice);
        Ok(candidates
            .iter()
            .any(|blob| same_file::is_same_file(path, blob).unwrap_or(false)))
    }
}

/// Makes `dest` a hard link to `blob`, replacing what is there.
fn link(blob: &Path, dest: &Path) -> Result<()> {
    let dir = dest.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(io_error(dir))?;
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    // Linked under another name first, so that the destination is never
    // missing or half there.
    let tmp = dir.join(format!(".{name}.mvault-{}", std::process::id()));
    remove_if_exists(&tmp)?;
    fs::hard_link(blob, &tmp).map_err(|error| {
        if error.kind() == io::ErrorKind::CrossesDevices {
            Error::Invalid(format!(
                "{} and {} are on different filesystems; a placed file is a hard link to its blob \
                 (set blobs in the settings to a directory on the filesystem of the workspace)",
                blob.display(),
                dir.display()
            ))
        } else {
            io_error(dest)(error)
        }
    })?;
    let renamed = fs::rename(&tmp, dest).or_else(|_| {
        // Where a rename does not replace an existing file.
        fs::remove_file(dest)?;
        fs::rename(&tmp, dest)
    });
    renamed.map_err(|error| {
        let _ = fs::remove_file(&tmp);
        io_error(dest)(error)
    })
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(io_error(path)(error)),
        _ => Ok(()),
    }
}

/// The directories files are placed in, without those inside another one.
/// The workspace itself is the empty string.
fn top_dirs(dests: &BTreeSet<&str>) -> Vec<String> {
    let dirs: BTreeSet<&str> = dests
        .iter()
        .map(|dest| dest.rsplit_once('/').map_or("", |(dir, _)| dir))
        .collect();
    let mut top: Vec<String> = Vec::new();
    // Sorted, so a directory comes before those inside it.
    for dir in dirs {
        let inside = top
            .iter()
            .any(|other| other.is_empty() || dir.starts_with(&format!("{other}/")));
        if !inside {
            top.push(dir.to_owned());
        }
    }
    top
}

/// Lists the regular files below `dir` (relative to `root`, with `/`),
/// leaving out mvault's own directory and anything that is not a plain file.
fn walk(root: &Path, dir: &str, files: &mut Vec<String>) -> Result<()> {
    let path = root.join(dir);
    let entries = match fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(&path)(error)),
    };
    for entry in entries {
        let entry = entry.map_err(io_error(&path))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let relative = if dir.is_empty() {
            name
        } else {
            format!("{dir}/{name}")
        };
        if relative == STATE_DIR {
            continue;
        }
        let kind = entry.file_type().map_err(io_error(&path))?;
        if kind.is_dir() {
            walk(root, &relative, files)?;
        } else if kind.is_file() {
            files.push(relative);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
