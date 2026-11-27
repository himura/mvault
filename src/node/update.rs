//! Resolving the models of the manifest again, for revisions that move: a
//! branch or tag of a repository, the latest version of a model.

use log::debug;

use super::lock::Lock;
use super::manifest::{Manifest, Model};
use super::plan::{describe_model, plan_resolved};
use super::resolve::{Access, is_commit, resolve};
use super::workspace::Workspace;
use crate::error::{Error, Result};
use crate::source::Source;

/// What became of one model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It stands for another commit or version now.
    Moved {
        /// What it stood for before.
        before: String,
        /// What it stands for now.
        after: String,
    },
    /// It still stands for the same commit or version.
    Unchanged(String),
    /// The lock had no resolution for it; now it has.
    Resolved(String),
    /// The manifest names a commit or a version: there is nothing to
    /// resolve again.
    Pinned,
}

/// One model the update looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updated {
    /// The model, as people know it.
    pub model: String,
    /// What became of it.
    pub outcome: Outcome,
}

/// Resolves again the models of the manifest that `sources` match, or all
/// of them, and writes the lock file. Nothing is fetched or placed; the next
/// sync does that.
///
/// # Errors
///
/// Returns an error, writing nothing, if a source matches no model of the
/// manifest, a model cannot be resolved, or what the manifest asks for is
/// not there in the new resolution.
pub fn update(
    workspace: &Workspace,
    access: &Access<'_>,
    sources: &[Source],
) -> Result<Vec<Updated>> {
    let _workspace = workspace.lock(true)?;
    let manifest = Manifest::load(&workspace.root, &workspace.defaults())?;
    for source in sources {
        if !manifest
            .models
            .iter()
            .any(|model| matches(source, &model.source))
        {
            return Err(Error::Invalid(format!(
                "the manifest has no model {source}"
            )));
        }
    }
    let lock_path = workspace.lock_path();
    let mut lock = Lock::load(&lock_path)?;

    let mut models: Vec<&Model> = Vec::new();
    for model in &manifest.models {
        let wanted =
            sources.is_empty() || sources.iter().any(|source| matches(source, &model.source));
        if wanted && !models.iter().any(|other| other.key() == model.key()) {
            models.push(model);
        }
    }
    let mut updated = Vec::new();
    let mut changed = false;
    for model in models {
        let (source, revision) = model.key();
        let before = lock
            .get(&source, revision)
            .map(|locked| locked.resolved.clone());
        let outcome = if is_pinned(model) && before.is_some() {
            debug!("{source}: the manifest names a commit or version; nothing to resolve again");
            Outcome::Pinned
        } else {
            debug!("{source}: resolving again");
            let locked = resolve(model, access)?;
            let after = locked.resolved.clone();
            match before {
                // The lock may know digests the upstream did not announce.
                Some(before) if before == after => Outcome::Unchanged(after),
                Some(before) => {
                    lock.insert(locked);
                    changed = true;
                    Outcome::Moved { before, after }
                }
                None => {
                    lock.insert(locked);
                    changed = true;
                    Outcome::Resolved(after)
                }
            }
        };
        updated.push(Updated {
            model: describe_model(&source, revision),
            outcome,
        });
    }
    if changed {
        // A new commit may lack a file the manifest names; sync would
        // fail on it, so the lock is left as it was.
        plan_resolved(&manifest, &lock)?;
        lock.save(&lock_path)?;
    }
    Ok(updated)
}

/// Whether the manifest names a commit or a version, which a new
/// resolution cannot move.
fn is_pinned(model: &Model) -> bool {
    match &model.source {
        Source::HuggingFace { .. } => model.revision.as_deref().is_some_and(is_commit),
        Source::Civitai { version_id, .. } => version_id.is_some(),
    }
}

/// Whether `given` on the command line names the model `source`: a model
/// without a version names every version of it.
fn matches(given: &Source, source: &Source) -> bool {
    match (given, source) {
        (
            Source::Civitai {
                host,
                model_id,
                version_id: None,
            },
            Source::Civitai {
                host: other_host,
                model_id: other_id,
                ..
            },
        ) => host == other_host && model_id == other_id,
        _ => given == source,
    }
}
