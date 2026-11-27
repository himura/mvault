//! Making the workspace match the manifest: resolve what is new, fetch the
//! blobs this node lacks, and hard-link them into place.
//!
//! A placed file is a hard link to a blob, and that is also how mvault knows
//! its own files: a file that is the same file as a blob is managed, any
//! other file belongs to the user and is left alone. Which managed files may
//! be removed is decided by the record of placed files alone.
//!
//! A sync first looks at the workspace and decides what to do (`survey`),
//! then does it. A dry run stops after the first part, so it shows what a
//! real run would do.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use log::debug;

use super::gc::gc_lock;
use super::lock::Lock;
use super::manifest::Manifest;
use super::placed::{Placed, Stamp, hash_file};
use super::plan::{Placement, plan, unresolved};
use super::resolve::{Access, fill_digests, parse_source, resolve};
use super::workspace::Workspace;
use crate::download::{Downloaded, Progress, Request};
use crate::error::{Error, Result};
use crate::fsutil::{io_error, lock_file};
use crate::gc::blob_at;
use crate::hash::Sha256;
use crate::source::Source;
use crate::store::BlobStore;

/// Prefix of the temporary links a sync makes next to the blobs.
pub(crate) const LINK_PREFIX: &str = "link-";

/// How to sync.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Options {
    /// Only report what would be done.
    pub dry_run: bool,
    /// Fail rather than resolve models the lock file does not have.
    pub locked: bool,
    /// Remove recorded files the manifest no longer places.
    pub prune: bool,
    /// Replace files that are in the way, and blobs that were written to.
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
    /// Recorded files removed.
    pub pruned: usize,
    /// Destinations taken by files mvault does not manage.
    pub conflicts: Vec<String>,
    /// Destinations whose blob was written to. Without `force` they are
    /// left as they are.
    pub damaged: Vec<String>,
    /// Recorded files the manifest no longer places, left in place for
    /// want of `prune`.
    pub dropped: Vec<String>,
    /// Recorded files the manifest no longer places that `prune` did not
    /// remove, because their blob was written to.
    pub kept: Vec<String>,
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
    /// The blob of a file was written to: its content is no longer what
    /// its digest says.
    Damaged(&'a str),
    /// A recorded file the manifest no longer places was removed.
    Pruned(&'a str),
    /// A recorded file the manifest no longer places was not removed,
    /// because its blob was written to.
    Kept(&'a str),
}

/// What to do with a file the manifest places.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// Nothing is there: link it.
    Place,
    /// A managed file with other content, or with `force` any file, is
    /// there: link over it.
    Replace,
    /// It is in place.
    Keep,
    /// Something mvault does not manage is there.
    Conflict,
    /// Its blob was written to, and without `force` it is left alone.
    Damaged,
}

impl Action {
    /// What the action does, for the log.
    fn describe(self, options: Options) -> &'static str {
        match self {
            Self::Keep => "in place",
            Self::Place => "nothing there; to link",
            Self::Replace if options.force => "to link over what is there (--force)",
            Self::Replace => "a managed file with other content is there; to link over it",
            Self::Conflict => "taken by a file mvault does not manage; left alone",
            Self::Damaged => "its blob was written to; left alone",
        }
    }
}

/// What to do with a recorded file the manifest no longer places.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fate {
    /// Remove it (with `prune`).
    Remove,
    /// Leave it, and its entry (without `prune`).
    Leave,
    /// Its blob was written to: leave it, and its entry.
    Damaged,
    /// It is gone or no longer a link to its blob: forget it.
    Forget,
}

impl Fate {
    /// What becomes of the file, for the log.
    fn describe(self) -> &'static str {
        match self {
            Self::Remove => "to remove (--prune)",
            Self::Leave => "left in place without --prune",
            Self::Damaged => "its blob was written to",
            Self::Forget => "gone or not a link to a blob, so forgotten",
        }
    }
}

/// What a sync is going to do.
#[derive(Debug, Clone, Default)]
pub(crate) struct Survey {
    /// What to do with each placement, by its index.
    pub(crate) actions: Vec<Action>,
    /// The placements whose blob is to be fetched, one per blob.
    pub(crate) fetch: Vec<usize>,
    /// Blobs whose content no longer has their digest.
    pub(crate) damaged: BTreeSet<Sha256>,
    /// Recorded files the manifest no longer places, and what to do with
    /// them.
    pub(crate) dropped: Vec<(String, Fate)>,
    /// Blobs that were touched but still have their digest, with their
    /// stamp now.
    pub(crate) restamped: BTreeMap<Sha256, Stamp>,
    /// Recorded files the manifest no longer places that are links to
    /// another blob than the recorded one, with that blob.
    pub(crate) corrected: Vec<(String, Sha256)>,
}

/// Makes `workspace` match its manifest.
///
/// # Errors
///
/// Returns an error if the manifest or lock file is not valid, a model
/// cannot be resolved, or a blob cannot be fetched or does not have the
/// digest of the lock file. Destinations taken by other files and damaged
/// blobs are not an error here; they are listed in the report.
pub fn sync(
    workspace: &Workspace,
    access: &Access<'_>,
    options: Options,
    on_event: &mut dyn FnMut(Event<'_>),
) -> Result<Report> {
    // One command at a time changes the workspace: the lock file and the
    // record are read and written under this lock.
    let _workspace = workspace.lock(!options.dry_run)?;
    // Held to the end: a garbage collection must not take a blob away
    // between its fetch and its first link.
    let _in_use = if options.dry_run {
        None
    } else {
        Some(gc_lock(&workspace.store, false)?)
    };
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
    workspace.check_dests(placements.iter().map(|placement| placement.dest.as_str()))?;
    let record_path = workspace.placed_path();
    let mut record = Placed::load(&record_path)?;
    let survey = survey(workspace, &placements, &record, options)?;

    if options.dry_run {
        tell(&survey, &placements, &mut report, on_event);
        report.fetched = survey.fetch.len();
        return Ok(report);
    }

    // A blob that was written to is taken out of the store; the files
    // linked to it keep the content they were given.
    if options.force {
        for digest in &survey.damaged {
            debug!("blob {digest} was written to; --force takes it out of the store");
            workspace.store.remove(digest).map_err(Error::Store)?;
        }
    }

    for (position, index) in survey.fetch.iter().enumerate() {
        if is_stored(workspace, &placements[*index]) {
            // The same file, placed twice, was fetched for the other place.
            debug!(
                "{}: its blob was fetched for another destination",
                placements[*index].dest
            );
            continue;
        }
        on_event(Event::Fetching {
            index: position + 1,
            count: survey.fetch.len(),
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

    record_placements(workspace, &placements, &survey, &mut record)?;
    record.save(&record_path)?;

    for (placement, action) in placements.iter().zip(&survey.actions) {
        let dest = workspace.root.join(&placement.dest);
        match action {
            Action::Keep => report.unchanged += 1,
            Action::Conflict => {
                on_event(Event::Conflict(&placement.dest));
                report.conflicts.push(placement.dest.clone());
            }
            Action::Damaged => {
                on_event(Event::Damaged(&placement.dest));
                report.damaged.push(placement.dest.clone());
            }
            Action::Place | Action::Replace => {
                let digest = placement.sha256.ok_or_else(|| {
                    Error::Invalid(format!("{}: the digest is still unknown", placement.dest))
                })?;
                link(&workspace.store, &workspace.store.blob_path(&digest), &dest)?;
                on_event(Event::Linked(&placement.dest));
                report.linked += 1;
            }
        }
    }

    settle_dropped(
        workspace,
        &survey,
        options,
        &mut record,
        &mut report,
        on_event,
    )?;
    record.save(&record_path)?;
    Ok(report)
}

/// Writes down the files about to be linked, and the new stamps of blobs
/// that were touched.
fn record_placements(
    workspace: &Workspace,
    placements: &[Placement],
    survey: &Survey,
    record: &mut Placed,
) -> Result<()> {
    // Written down before the links are made: a file linked but not
    // recorded could never be pruned, while an entry for a link that was
    // not made is forgotten by the next sync.
    for (dest, digest) in &survey.corrected {
        let blob = workspace.store.blob_path(digest);
        if let Some(stamp) = Stamp::of(&blob)? {
            record.insert(dest.clone(), stamp.entry(*digest));
        }
    }
    for (digest, stamp) in &survey.restamped {
        let dests: Vec<String> = record
            .iter()
            .filter(|(_, entry)| entry.sha256 == *digest)
            .map(|(dest, _)| dest.to_owned())
            .collect();
        for dest in dests {
            record.insert(dest, stamp.entry(*digest));
        }
    }
    for (placement, action) in placements.iter().zip(&survey.actions) {
        match action {
            Action::Place | Action::Replace | Action::Keep => {
                let Some(digest) = placement.sha256 else {
                    continue;
                };
                let blob = workspace.store.blob_path(&digest);
                let stamp = Stamp::of(&blob)?.ok_or_else(|| {
                    Error::Invalid(format!("blob {digest} is gone from {}", blob.display()))
                })?;
                record.insert(placement.dest.clone(), stamp.entry(digest));
            }
            Action::Conflict => {
                record.remove(&placement.dest);
            }
            Action::Damaged => {}
        }
    }
    Ok(())
}

/// Removes, keeps or forgets the recorded files the manifest no longer
/// places, as the survey decided.
fn settle_dropped(
    workspace: &Workspace,
    survey: &Survey,
    options: Options,
    record: &mut Placed,
    report: &mut Report,
    on_event: &mut dyn FnMut(Event<'_>),
) -> Result<()> {
    for (dest, fate) in &survey.dropped {
        match fate {
            Fate::Remove => {
                let path = workspace.root.join(dest);
                remove_if_exists(&path)?;
                record.remove(dest);
                on_event(Event::Pruned(dest));
                report.pruned += 1;
            }
            Fate::Leave => report.dropped.push(dest.clone()),
            Fate::Damaged if options.force => {
                // Its blob is out of the store: it is an ordinary file now.
                record.remove(dest);
            }
            Fate::Damaged => {
                on_event(Event::Kept(dest));
                report.kept.push(dest.clone());
            }
            Fate::Forget => {
                record.remove(dest);
            }
        }
    }
    Ok(())
}

/// Tells what a dry run found.
fn tell(
    survey: &Survey,
    placements: &[Placement],
    report: &mut Report,
    on_event: &mut dyn FnMut(Event<'_>),
) {
    for (placement, action) in placements.iter().zip(&survey.actions) {
        match action {
            Action::Keep => report.unchanged += 1,
            Action::Conflict => {
                on_event(Event::Conflict(&placement.dest));
                report.conflicts.push(placement.dest.clone());
            }
            Action::Damaged => {
                on_event(Event::Damaged(&placement.dest));
                report.damaged.push(placement.dest.clone());
            }
            Action::Place | Action::Replace => {
                on_event(Event::Linked(&placement.dest));
                report.linked += 1;
            }
        }
    }
    for (dest, fate) in &survey.dropped {
        match fate {
            Fate::Remove => {
                on_event(Event::Pruned(dest));
                report.pruned += 1;
            }
            Fate::Leave => report.dropped.push(dest.clone()),
            Fate::Damaged => {
                on_event(Event::Kept(dest));
                report.kept.push(dest.clone());
            }
            Fate::Forget => {}
        }
    }
}

/// Looks at the destinations and the blobs, and decides what a sync does.
/// Changes nothing.
///
/// # Errors
///
/// Returns an error if a file or blob cannot be looked at.
pub(crate) fn survey(
    workspace: &Workspace,
    placements: &[Placement],
    record: &Placed,
    options: Options,
) -> Result<Survey> {
    let store = &workspace.store;
    let mut blobs = Blobs::new(store, record);
    let mut managed = Managed::new(store);
    let mut survey = Survey::default();

    let mut wanted_blobs = BTreeSet::new();
    let mut unknown = Vec::new();
    for (index, placement) in placements.iter().enumerate() {
        let dest = workspace.root.join(&placement.dest);
        let blob = match placement.sha256 {
            Some(digest) => Some((digest, blobs.state(digest)?)),
            None => None,
        };
        let damaged = matches!(blob, Some((_, BlobState::Damaged)));
        let stored = matches!(blob, Some((_, BlobState::Stored)));
        if damaged && !options.force {
            debug!("{}: {}", placement.dest, Action::Damaged.describe(options));
            survey.actions.push(Action::Damaged);
            continue;
        }
        let needs_fetch = match blob {
            Some((digest, _)) => !stored && wanted_blobs.insert(digest),
            None => {
                !unknown.iter().any(|other| same_file(other, placement)) && {
                    unknown.push(placement.clone());
                    true
                }
            }
        };
        if needs_fetch {
            debug!("{}: its blob is not stored; to fetch", placement.dest);
            survey.fetch.push(index);
        }

        let in_place = match blob {
            Some((digest, BlobState::Stored)) => is_same(&dest, &store.blob_path(&digest))?,
            _ => false,
        };
        let action = if in_place {
            Action::Keep
        } else {
            match what_is_at(&dest)? {
                At::Nothing => Action::Place,
                At::Other => {
                    if options.force {
                        Action::Replace
                    } else {
                        Action::Conflict
                    }
                }
                At::File => {
                    // Not necessarily the recorded blob: a sync that stopped
                    // between writing down a replacement and linking it
                    // leaves the blob before.
                    let recorded = record.get(&placement.dest).map(|entry| entry.sha256);
                    let is_managed = managed.blob_of(&dest, recorded)?.is_some();
                    if is_managed || options.force {
                        Action::Replace
                    } else {
                        Action::Conflict
                    }
                }
            }
        };
        debug!("{}: {}", placement.dest, action.describe(options));
        survey.actions.push(action);
    }

    let placed: BTreeSet<&str> = placements
        .iter()
        .map(|placement| placement.dest.as_str())
        .collect();
    for (dest, entry) in record.iter() {
        if placed.contains(dest) {
            continue;
        }
        let path = workspace.root.join(dest);
        let fate = match managed.blob_of(&path, Some(entry.sha256))? {
            None => Fate::Forget,
            Some(digest) => {
                if digest != entry.sha256 {
                    debug!(
                        "{dest}: recorded with blob {}, but linked to blob {digest}",
                        entry.sha256
                    );
                    survey.corrected.push((dest.to_owned(), digest));
                }
                if blobs.state(digest)? == BlobState::Damaged {
                    Fate::Damaged
                } else if options.prune {
                    Fate::Remove
                } else {
                    Fate::Leave
                }
            }
        };
        debug!("{dest}: no longer placed; {}", fate.describe());
        survey.dropped.push((dest.to_owned(), fate));
    }
    survey.damaged = blobs.damaged();
    survey.restamped = blobs.restamped;
    Ok(survey)
}

/// The state of a blob, as far as the sync is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlobState {
    /// The store does not have it.
    Missing,
    /// The store has it with its content.
    Stored,
    /// The store has it, but its content was written to.
    Damaged,
}

/// Finds out the state of blobs, once each. A blob is read again only if
/// its stamp differs from the one recorded.
struct Blobs<'a> {
    store: &'a BlobStore,
    recorded: HashMap<Sha256, Stamp>,
    known: HashMap<Sha256, BlobState>,
    restamped: BTreeMap<Sha256, Stamp>,
}

impl<'a> Blobs<'a> {
    fn new(store: &'a BlobStore, record: &Placed) -> Self {
        let mut recorded = HashMap::new();
        for (_, entry) in record.iter() {
            recorded.entry(entry.sha256).or_insert(Stamp {
                size: entry.size,
                mtime_ns: entry.mtime_ns,
            });
        }
        Self {
            store,
            recorded,
            known: HashMap::new(),
            restamped: BTreeMap::new(),
        }
    }

    fn state(&mut self, digest: Sha256) -> Result<BlobState> {
        if let Some(state) = self.known.get(&digest) {
            return Ok(*state);
        }
        let path = self.store.blob_path(&digest);
        let state = match Stamp::of(&path)? {
            None => BlobState::Missing,
            Some(stamp) => match self.recorded.get(&digest) {
                Some(recorded) if *recorded != stamp => {
                    debug!(
                        "blob {digest} changed its size or time since it was recorded; hashing it"
                    );
                    if hash_file(&path)? == digest {
                        debug!("blob {digest} still has its digest");
                        self.restamped.insert(digest, stamp);
                        BlobState::Stored
                    } else {
                        debug!("blob {digest} no longer has its digest");
                        BlobState::Damaged
                    }
                }
                _ => BlobState::Stored,
            },
        };
        self.known.insert(digest, state);
        Ok(state)
    }

    fn damaged(&self) -> BTreeSet<Sha256> {
        self.known
            .iter()
            .filter(|(_, state)| **state == BlobState::Damaged)
            .map(|(digest, _)| *digest)
            .collect()
    }
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
        debug!("{}: not in the lock file; resolving it", model.source);
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
            debug!("{source}: digests the upstream did not announce are known now");
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
    let _guard = lock_file(&lock_path)?;

    if let Some(digest) = placement.sha256 {
        if store.contains(&digest) {
            debug!("blob {digest} was stored by another process meanwhile");
            return Ok(digest);
        }
        for remote in access.usable_remotes() {
            debug!("asking mirror {} for blob {digest}", remote.redacted());
            let fetched = remote.fetch_blob(
                access.http,
                &digest,
                placement.size,
                &partial,
                &mut |progress| on_event(Event::Progress(progress)),
            );
            match fetched {
                Ok(Some(downloaded)) if downloaded.sha256 == digest => {
                    debug!("storing blob {digest} from mirror {}", remote.redacted());
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
                Ok(None) => debug!("mirror {} does not have blob {digest}", remote.redacted()),
                Err(error) => access.mirror_failed(remote, &error),
            }
        }
    }

    let source = parse_source(&placement.source)?;
    if !workspace.config.allow_official {
        debug!("fetching from the upstream is not allowed by the settings (allow_official)");
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
        debug!(
            "{}: no digest known, so a partial file is not continued",
            placement.dest
        );
        remove_if_exists(&partial)?;
    }
    debug!(
        "{}: fetching {} of {} from the upstream",
        placement.dest, placement.path, placement.source
    );
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
        debug!(
            "{}: {reason}; removing {}",
            placement.dest,
            partial.display()
        );
        remove_if_exists(&partial)?;
        return Err(Error::Upstream { url, reason });
    }
    debug!("storing blob {} from the upstream", downloaded.sha256);
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
            let hub = (access.hub)(host)?;
            let url = hub.file_url(repo, &placement.resolved, &placement.path);
            let request = Request {
                url: &url,
                headers: &hub.headers_for(&url),
                size: placement.size,
            };
            let downloaded = access.http.download(&request, partial, &mut progress)?;
            Ok((url, downloaded))
        }
        Source::Civitai { host, .. } => {
            let site = (access.site)(host)?;
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
                size: placement.size,
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

/// What is at a destination, before it is compared with any blob.
enum At {
    /// Nothing.
    Nothing,
    /// A regular file.
    File,
    /// A directory, a symbolic link or anything else that is not a regular
    /// file.
    Other,
}

fn what_is_at(dest: &Path) -> Result<At> {
    match fs::symlink_metadata(dest) {
        Ok(metadata) if metadata.is_file() => Ok(At::File),
        Ok(_) => Ok(At::Other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(At::Nothing),
        Err(error) => Err(io_error(dest)(error)),
    }
}

/// Whether `path` is a regular file and the same file as `blob`. Either
/// may be missing.
fn is_same(path: &Path, blob: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(io_error(path)(error)),
    }
    match same_file::is_same_file(path, blob) {
        Ok(same) => Ok(same),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(path)(error)),
    }
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

    /// The digest of the blob that the regular file at `path` is a hard
    /// link to, if any.
    fn find(&mut self, path: &Path) -> Result<Option<Sha256>> {
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
            .find(|blob| same_file::is_same_file(path, blob).unwrap_or(false))
            .and_then(|blob| blob_at(self.store, blob)))
    }

    /// The digest of the blob that the file at `path` is a hard link to:
    /// `recorded` if it is that one, any other otherwise. `None` if it is
    /// missing or not a hard link to a blob.
    fn blob_of(&mut self, path: &Path, recorded: Option<Sha256>) -> Result<Option<Sha256>> {
        if let Some(digest) = recorded
            && is_same(path, &self.store.blob_path(&digest))?
        {
            return Ok(Some(digest));
        }
        match what_is_at(path)? {
            At::File => self.find(path),
            At::Nothing | At::Other => Ok(None),
        }
    }
}

/// Makes `dest` a hard link to `blob`, replacing what is there.
///
/// The link is made under a temporary name next to the blobs and renamed
/// into place, so that the destination is never missing or half there, and
/// a sync that stops leaves nothing behind in the workspace. Garbage
/// collection removes what it leaves next to the blobs.
fn link(store: &BlobStore, blob: &Path, dest: &Path) -> Result<()> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = dest.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(io_error(dir))?;
    let tmp_dir = store.tmp_dir();
    fs::create_dir_all(tmp_dir).map_err(io_error(tmp_dir))?;
    let tmp = tmp_dir.join(format!(
        "{LINK_PREFIX}{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    remove_if_exists(&tmp)?;
    fs::hard_link(blob, &tmp).map_err(io_error(&tmp))?;
    let renamed = fs::rename(&tmp, dest).or_else(|error| {
        // Where a rename does not replace an existing file. Elsewhere a
        // failed rename leaves the destination as it was.
        if cfg!(unix) || !dest.is_file() {
            return Err(error);
        }
        fs::remove_file(dest)?;
        fs::rename(&tmp, dest)
    });
    renamed.map_err(|error| {
        let _ = fs::remove_file(&tmp);
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
    })
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(io_error(path)(error)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
