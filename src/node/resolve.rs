//! Finding out what a model of the manifest stands for: from a mirror's
//! snapshot if one has it, from the upstream otherwise.

use std::cell::RefCell;
use std::collections::HashSet;

use log::debug;

use super::lock::{LockedFile, LockedModel};
use super::manifest::Model;
use super::remote::Remote;
use crate::download::Client;
use crate::error::{Error, Result};
use crate::fsutil::check_relative;
use crate::snapshot::{self, Snapshot};
use crate::source::{Defaults, Reference, Source};
use crate::upstream::civitai::Site;
use crate::upstream::hf::Hub;

/// What a node reaches out to.
pub struct Access<'a> {
    /// The HTTP client.
    pub http: &'a Client,
    /// The mirrors, in the order to try them.
    pub remotes: &'a [Remote],
    /// The client for the hub on a host.
    pub hub: &'a dyn Fn(&str) -> Result<Hub>,
    /// The client for the site on a host.
    pub site: &'a dyn Fn(&str) -> Result<Site>,
    /// Problems that did not stop the work, such as a mirror that could not
    /// be read while another one could.
    pub warnings: RefCell<Vec<String>>,
    /// The mirrors that could not be read in this run.
    pub down: RefCell<HashSet<String>>,
}

impl std::fmt::Debug for Access<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Access")
            .field("remotes", &self.remotes)
            .finish_non_exhaustive()
    }
}

impl Access<'_> {
    pub(crate) fn warn(&self, message: String) {
        self.warnings.borrow_mut().push(message);
    }

    /// The mirrors to try, without those that failed earlier in this run.
    pub(crate) fn usable_remotes(&self) -> impl Iterator<Item = &Remote> {
        self.remotes
            .iter()
            .filter(|remote| !self.down.borrow().contains(&remote.to_string()))
    }

    /// Notes that a mirror could not be read: it is not tried again in this
    /// run, and warned about once.
    pub(crate) fn mirror_failed(&self, remote: &Remote, error: &Error) {
        if self.down.borrow_mut().insert(remote.to_string()) {
            self.warn(format!(
                "mirror {remote}: {error} (not tried again until the next run)"
            ));
        }
    }
}

/// Resolves a model of the manifest: the commit or version it stands for
/// and every file the upstream lists there.
///
/// # Errors
///
/// Returns an error if no mirror has a snapshot of the model and the
/// upstream cannot be asked either.
pub fn resolve(model: &Model, access: &Access<'_>) -> Result<LockedModel> {
    let (source, revision) = model.key();
    let from_mirror = match &model.source {
        Source::HuggingFace { .. } => {
            let revision = revision.unwrap_or_default();
            if is_commit(revision) {
                read_snapshot(&model.source, revision, access)
            } else {
                read_ref(&model.source, revision, access)
                    .and_then(|commit| read_snapshot(&model.source, &commit, access))
            }
        }
        Source::Civitai { version_id, .. } => match version_id {
            Some(version_id) => read_snapshot(&model.source, &version_id.to_string(), access),
            None => read_ref(&model.source, "latest", access)
                .and_then(|version| read_snapshot(&model.source, &version, access)),
        },
    };
    let by_mirror = from_mirror.is_some();
    if !by_mirror {
        debug!("{source}: no mirror has a snapshot; asking the upstream");
    }
    let (resolved, files) = match from_mirror {
        Some(snapshot) => (
            snapshot.revision,
            snapshot
                .files
                .into_iter()
                .map(|file| LockedFile {
                    path: file.path,
                    file_id: file.file_id,
                    size: file.size,
                    sha256: file.sha256,
                    primary: file.primary,
                })
                .collect(),
        ),
        None => from_upstream(model, access).map_err(|error| {
            if access.remotes.is_empty() {
                error
            } else {
                Error::Invalid(format!(
                    "{source}: no mirror has it, and the upstream could not be asked: {error}"
                ))
            }
        })?,
    };
    debug!(
        "{source}: resolved to {resolved} by {}",
        if by_mirror {
            "a mirror's snapshot"
        } else {
            "the upstream"
        }
    );
    Ok(LockedModel {
        source,
        revision: revision.map(str::to_owned),
        resolved,
        files,
    })
}

/// Fills in digests the lock does not have yet from a mirror's snapshot of
/// the resolved revision. Returns whether anything was filled in.
///
/// # Errors
///
/// Returns an error if the source in the lock is not valid.
pub fn fill_digests(locked: &mut LockedModel, access: &Access<'_>) -> Result<bool> {
    if locked.files.iter().all(|file| file.sha256.is_some()) {
        return Ok(false);
    }
    let source = parse_source(&locked.source)?;
    let Some(snapshot) = read_snapshot(&source, &locked.resolved, access) else {
        return Ok(false);
    };
    let mut filled = false;
    for file in &mut locked.files {
        if file.sha256.is_some() {
            continue;
        }
        let known = snapshot
            .files
            .iter()
            .find(|other| other.path == file.path && other.file_id == file.file_id)
            .and_then(|other| other.sha256);
        if let Some(digest) = known {
            debug!(
                "{}: digest of {} taken from a mirror's snapshot",
                locked.source, file.path
            );
            file.sha256 = Some(digest);
            filled = true;
        }
    }
    Ok(filled)
}

/// The source a lock entry names.
pub(crate) fn parse_source(source: &str) -> Result<Source> {
    Reference::parse(source, &Defaults::default())
        .map(|reference| reference.source)
        .map_err(|e| Error::Invalid(e.to_string()))
}

fn is_commit(revision: &str) -> bool {
    revision.len() == 40
        && revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The first answer any mirror gives for `path`. A mirror that cannot be
/// read is noted and passed over.
fn read_first(path: &str, access: &Access<'_>) -> Option<Vec<u8>> {
    for remote in access.usable_remotes() {
        match remote.read(access.http, path) {
            Ok(Some(content)) => {
                debug!("mirror {} has {path}", remote.redacted());
                return Some(content);
            }
            Ok(None) => debug!("mirror {} does not have {path}", remote.redacted()),
            Err(error) => access.mirror_failed(remote, &error),
        }
    }
    None
}

fn read_ref(source: &Source, name: &str, access: &Access<'_>) -> Option<String> {
    let content = read_first(&snapshot::ref_path(source, name), access)?;
    let revision = String::from_utf8(content).ok()?.trim().to_owned();
    if !revision.is_empty() {
        debug!("{source}: {name} stands for {revision}");
    }
    (!revision.is_empty()).then_some(revision)
}

fn read_snapshot(source: &Source, revision: &str, access: &Access<'_>) -> Option<Snapshot> {
    let path = snapshot::snapshot_path(source, revision);
    let content = read_first(&path, access)?;
    match serde_json::from_slice::<Snapshot>(&content) {
        Ok(snapshot) if snapshot.schema == snapshot::SCHEMA && snapshot.revision == revision => {
            // A mirror is read over the network: a path in its snapshot
            // must not lead a placement out of its destination.
            if let Some(bad) = snapshot
                .files
                .iter()
                .find(|file| check_relative(&file.path).is_err())
            {
                access.warn(format!("{path}: not usable (unsafe path {:?})", bad.path));
                return None;
            }
            Some(snapshot)
        }
        Ok(snapshot) => {
            access.warn(format!(
                "{path}: not usable (schema {}, revision {})",
                snapshot.schema, snapshot.revision
            ));
            None
        }
        Err(error) => {
            access.warn(format!("{path}: {error}"));
            None
        }
    }
}

fn from_upstream(model: &Model, access: &Access<'_>) -> Result<(String, Vec<LockedFile>)> {
    match &model.source {
        Source::HuggingFace { host, repo } => {
            let hub = (access.hub)(host)?;
            let revision = model.revision.as_deref().unwrap_or_default();
            let info = hub.repo_info(access.http, repo, revision)?;
            let files = info
                .files
                .into_iter()
                .map(|file| LockedFile {
                    path: file.path,
                    file_id: None,
                    size: file.size,
                    sha256: file.sha256,
                    primary: false,
                })
                .collect();
            Ok((info.commit, files))
        }
        Source::Civitai {
            host,
            model_id,
            version_id,
        } => {
            let site = (access.site)(host)?;
            let version_id = match version_id {
                Some(version_id) => *version_id,
                None => site
                    .model(access.http, *model_id)?
                    .0
                    .ok_or_else(|| Error::Invalid(format!("{} has no versions", model.source)))?,
            };
            let version = site.version(access.http, version_id)?;
            if version.model_id != *model_id {
                return Err(Error::Invalid(format!(
                    "version {version_id} belongs to model {}, not {model_id}",
                    version.model_id
                )));
            }
            let files = version
                .files
                .into_iter()
                .map(|file| LockedFile {
                    path: file.name,
                    file_id: Some(file.id),
                    size: None,
                    sha256: file.sha256,
                    primary: file.primary,
                })
                .collect();
            Ok((version_id.to_string(), files))
        }
    }
}
