//! Looking at a workspace without changing it: `status` by model, `ls` by
//! file.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use super::lock::Lock;
use super::manifest::{LOCK_NAME, Manifest};
use super::placed::Placed;
use super::plan::{Placement, describe_model, plan_resolved};
use super::resolve::{Access, parse_source, read_snapshot};
use super::sync::{Action, Fate, Managed, Options, survey};
use super::update::matches;
use super::workspace::Workspace;
use crate::error::Result;
use crate::fsutil::io_error;
use crate::hash::Sha256;
use crate::source::Source;

/// How a model of the manifest stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelStatus {
    /// The model, as people know it.
    pub model: String,
    /// The commit or version the lock resolves it to; `None` if the lock
    /// has no resolution for it yet.
    pub resolved: Option<String>,
    /// Number of files the manifest places from it.
    pub files: usize,
    /// How many of those have their blob on this node.
    pub fetched: usize,
    /// What the mirrors have of it.
    pub mirror: OnMirror,
}

/// What the mirrors have of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnMirror {
    /// No mirror is set, or the model is not resolved.
    Unknown,
    /// No mirror has a snapshot of the resolved revision.
    Missing,
    /// A mirror has the snapshot, and this many of the placed files.
    Has(usize),
}

/// How each model of the manifest stands, in the order of the manifest;
/// with `filter`, only the models it names.
///
/// # Errors
///
/// Returns an error if the manifest or the lock is not valid, or the files
/// of a resolved model cannot be placed as the manifest says.
pub fn status(
    workspace: &Workspace,
    access: &Access<'_>,
    filter: Option<&Source>,
) -> Result<Vec<ModelStatus>> {
    let _workspace = workspace.lock(false)?;
    let manifest = Manifest::load(&workspace.root, &workspace.defaults())?;
    let lock = Lock::load(&workspace.lock_path())?;
    let (placements, _) = plan_resolved(&manifest, &lock)?;

    let mut seen = Vec::new();
    let mut statuses = Vec::new();
    for model in &manifest.models {
        if filter.is_some_and(|filter| !matches(filter, &model.source)) {
            continue;
        }
        let (source, revision) = model.key();
        if seen.contains(&(source.clone(), revision)) {
            continue;
        }
        seen.push((source.clone(), revision));
        let Some(locked) = lock.get(&source, revision) else {
            statuses.push(ModelStatus {
                model: describe_model(&source, revision),
                resolved: None,
                files: 0,
                fetched: 0,
                mirror: OnMirror::Unknown,
            });
            continue;
        };
        let own: Vec<&Placement> = placements
            .iter()
            .filter(|placement| {
                placement.source == source && placement.revision.as_deref() == revision
            })
            .collect();
        let fetched = own
            .iter()
            .filter(|placement| {
                placement
                    .sha256
                    .is_some_and(|digest| workspace.store.contains(&digest))
            })
            .count();
        let mirror = if access.remotes.is_empty() {
            OnMirror::Unknown
        } else {
            match read_snapshot(&parse_source(&source)?, &locked.resolved, access) {
                None => OnMirror::Missing,
                Some(snapshot) => OnMirror::Has(
                    own.iter()
                        .filter(|placement| {
                            snapshot.files.iter().any(|file| {
                                file.fetched
                                    && file.path == placement.path
                                    && file.file_id == placement.file_id
                            })
                        })
                        .count(),
                ),
            }
        };
        statuses.push(ModelStatus {
            model: describe_model(&source, revision),
            resolved: Some(locked.resolved.clone()),
            files: own.len(),
            fetched,
            mirror,
        });
    }
    Ok(statuses)
}

/// The state of a file `ls` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The manifest places it, and this node does not have its blob.
    NotFetched,
    /// The manifest places it, and this node has its blob, but it is not
    /// linked into place.
    Fetched,
    /// The manifest places it, and it is in place.
    Placed,
    /// The manifest places it, and a file mvault does not manage is there.
    Conflict,
    /// Its blob changed size or time since it was recorded: it may have
    /// been written to. `sync` finds out.
    Suspect,
    /// Recorded, but the manifest no longer places it: `sync --prune`
    /// removes it.
    Dropped,
    /// Neither placed nor recorded, but a link to a blob: placed before
    /// there was a record, say.
    Unrecorded,
    /// Neither placed nor recorded, and not a link to a blob: the user's.
    Unmanaged,
}

impl State {
    /// The word `ls` shows.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::NotFetched => "not fetched",
            Self::Fetched => "fetched",
            Self::Placed => "placed",
            Self::Conflict => "conflict",
            Self::Suspect => "damaged?",
            Self::Dropped => "dropped",
            Self::Unrecorded => "unrecorded",
            Self::Unmanaged => "unmanaged",
        }
    }
}

/// A file `ls` shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// Path relative to the workspace, with `/`.
    pub dest: String,
    /// Its state.
    pub state: State,
    /// Size in bytes, if known.
    pub size: Option<u64>,
    /// Digest of its content, if known.
    pub sha256: Option<Sha256>,
    /// The source of its model, for a file the manifest places.
    pub source: Option<String>,
}

/// What `ls` found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    /// The files, ordered by path.
    pub files: Vec<Listed>,
    /// Models of the manifest the lock has no resolution for yet; their
    /// files are not shown.
    pub unresolved: Vec<String>,
}

/// The files of the workspace below `under` (relative, with `/`; empty for
/// all) that the manifest places or the record has; with `untracked`, also
/// the other files in the directories where those are.
///
/// Looks the same way `sync` does, without reading any blob.
///
/// # Errors
///
/// Returns an error if the manifest, the lock or the record is not valid,
/// or a file cannot be looked at.
pub fn ls(workspace: &Workspace, under: &str, untracked: bool) -> Result<Listing> {
    let _workspace = workspace.lock(false)?;
    let manifest = Manifest::load(&workspace.root, &workspace.defaults())?;
    let lock = Lock::load(&workspace.lock_path())?;
    let (placements, missing) = plan_resolved(&manifest, &lock)?;
    workspace.check_dests(placements.iter().map(|placement| placement.dest.as_str()))?;
    let record = Placed::load(&workspace.placed_path())?;
    let survey = survey(workspace, &placements, &record, Options::default(), false)?;

    let mut files = Vec::new();
    for (placement, action) in placements.iter().zip(&survey.actions) {
        let stored = placement
            .sha256
            .is_some_and(|digest| workspace.store.contains(&digest));
        let state = match action {
            Action::Keep => State::Placed,
            Action::Conflict => State::Conflict,
            Action::Damaged => State::Suspect,
            Action::Place | Action::Replace if stored => State::Fetched,
            Action::Place | Action::Replace => State::NotFetched,
        };
        files.push(Listed {
            dest: placement.dest.clone(),
            state,
            size: placement.size,
            sha256: placement.sha256,
            source: Some(placement.source.clone()),
        });
    }
    for (dest, fate) in &survey.dropped {
        let state = match fate {
            Fate::Remove | Fate::Leave => State::Dropped,
            Fate::Damaged => State::Suspect,
            // Gone, or no longer the blob's: as for any other file.
            Fate::Forget => continue,
        };
        let entry = record.get(dest);
        files.push(Listed {
            dest: dest.clone(),
            state,
            size: entry.map(|entry| entry.size),
            sha256: entry.map(|entry| entry.sha256),
            source: None,
        });
    }
    if untracked {
        files.extend(others(workspace, &manifest, &files)?);
    }
    files.retain(|file| is_below(&file.dest, under));
    files.sort_by(|a, b| a.dest.cmp(&b.dest));
    Ok(Listing {
        files,
        unresolved: missing
            .iter()
            .map(|model| {
                let (source, revision) = model.key();
                describe_model(&source, revision)
            })
            .collect(),
    })
}

/// The files in the directories of `known` that are not among them, nor
/// the manifest or the lock. Directories below those are not looked into.
fn others(workspace: &Workspace, manifest: &Manifest, known: &[Listed]) -> Result<Vec<Listed>> {
    let mut taken: BTreeSet<&str> = known.iter().map(|file| file.dest.as_str()).collect();
    taken.extend(manifest.files.iter().map(String::as_str));
    taken.insert(LOCK_NAME);
    let dirs: BTreeSet<&str> = known
        .iter()
        .map(|file| file.dest.rsplit_once('/').map_or("", |(dir, _)| dir))
        .collect();
    let mut managed = Managed::new(&workspace.store);
    let mut found = Vec::new();
    for dir in dirs {
        let path = workspace.root.join(dir);
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(&path)(error)),
        };
        for entry in entries {
            let entry = entry.map_err(io_error(&path))?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let dest = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            let file = entry.path();
            let metadata = fs::symlink_metadata(&file).map_err(io_error(&file))?;
            if !metadata.is_file() || taken.contains(dest.as_str()) {
                continue;
            }
            let blob = managed.blob_of(&file, None)?;
            found.push(Listed {
                state: if blob.is_some() {
                    State::Unrecorded
                } else {
                    State::Unmanaged
                },
                size: Some(metadata.len()),
                sha256: blob,
                source: None,
                dest,
            });
        }
    }
    Ok(found)
}

/// Whether `dest` is `under` or below it.
fn is_below(dest: &str, under: &str) -> bool {
    under.is_empty()
        || dest == under
        || dest
            .strip_prefix(under)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `path` (given on the command line, relative to `cwd`) relative to the
/// workspace at `root`, with `/`; empty for the workspace itself.
///
/// # Errors
///
/// Returns an error if the path is outside the workspace.
pub fn relative_to(root: &Path, cwd: &Path, path: &Path) -> Result<String> {
    let full = super::workspace::normalize(&cwd.join(path));
    let relative = full
        .strip_prefix(super::workspace::normalize(root))
        .map_err(|_| {
            crate::error::Error::Invalid(format!(
                "{} is outside the workspace {}",
                path.display(),
                root.display()
            ))
        })?;
    Ok(relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}
