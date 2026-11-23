//! Working out which file goes where, from the manifest and the lock.

use std::collections::BTreeMap;

use globset::GlobBuilder;

use super::lock::{Lock, LockedFile, LockedModel};
use super::manifest::{Manifest, Model, Select, What};
use crate::error::{Error, Result};
use crate::hash::Sha256;
use crate::mirror::exclude::Excludes;

/// One file to place in the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// Where to place it, relative to the workspace, with `/` separators.
    pub dest: String,
    /// The source of the model, as the lock names it.
    pub source: String,
    /// The revision of the model, as the lock names it.
    pub revision: Option<String>,
    /// The commit or version id the model is resolved to.
    pub resolved: String,
    /// Path of the file in its repository, or its file name.
    pub path: String,
    /// Id of the file, where the upstream gives files ids.
    pub file_id: Option<u64>,
    /// Size in bytes, if known.
    pub size: Option<u64>,
    /// Digest of the content, if known yet.
    pub sha256: Option<Sha256>,
}

/// The models of the manifest the lock has no resolution for.
#[must_use]
pub fn unresolved<'a>(manifest: &'a Manifest, lock: &Lock) -> Vec<&'a Model> {
    let mut missing: Vec<&Model> = Vec::new();
    for model in &manifest.models {
        let (source, revision) = model.key();
        if lock.get(&source, revision).is_none()
            && !missing.iter().any(|other| other.key() == model.key())
        {
            missing.push(model);
        }
    }
    missing
}

/// Every file the manifest places, ordered by destination.
///
/// # Errors
///
/// Returns an error if a model is not in the lock, if an entry names a file
/// its model does not have, or if two different files are to be placed at
/// the same path.
pub fn plan(manifest: &Manifest, lock: &Lock) -> Result<Vec<Placement>> {
    let mut placements: BTreeMap<String, Placement> = BTreeMap::new();
    for model in &manifest.models {
        let (source, revision) = model.key();
        let locked = lock.get(&source, revision).ok_or_else(|| {
            Error::Invalid(format!(
                "{source} is not in the lock file (run mvault sync without --locked)"
            ))
        })?;
        let selected = select(model, locked).map_err(|reason| {
            Error::Invalid(format!(
                "{}: model {source:?} at {}: {reason}",
                model.file, locked.resolved
            ))
        })?;
        for (dest, file) in selected {
            let placement = Placement {
                dest: dest.clone(),
                source: locked.source.clone(),
                revision: locked.revision.clone(),
                resolved: locked.resolved.clone(),
                path: file.path.clone(),
                file_id: file.file_id,
                size: file.size,
                sha256: file.sha256,
            };
            if let Some(other) = placements.get(&dest)
                && !same_file(other, &placement)
            {
                return Err(Error::Invalid(format!(
                    "{dest} is given twice: {} of {} and {} of {}",
                    other.path, other.source, placement.path, placement.source
                )));
            }
            placements.insert(dest, placement);
        }
    }
    // Sorted, so what is below a destination follows it closely.
    for dest in placements.keys() {
        let below = format!("{dest}/");
        if let Some((other, _)) = placements.range(below.clone()..).next()
            && other.starts_with(&below)
        {
            return Err(Error::Invalid(format!(
                "{dest} is both a file and a directory (of {other})"
            )));
        }
    }
    Ok(placements.into_values().collect())
}

fn same_file(a: &Placement, b: &Placement) -> bool {
    (&a.source, &a.revision, &a.path, a.file_id) == (&b.source, &b.revision, &b.path, b.file_id)
}

/// The files of `locked` that `model` places, with their destinations.
fn select<'a>(
    model: &Model,
    locked: &'a LockedModel,
) -> std::result::Result<Vec<(String, &'a LockedFile)>, String> {
    let mut selected = Vec::new();
    match &model.select {
        Select::Whole { dest, exclude } => {
            let excludes = Excludes::new(exclude).map_err(|e| e.to_string())?;
            for file in &locked.files {
                if !excludes.matches(&file.path) {
                    selected.push((join(dest, &file.path), file));
                }
            }
        }
        Select::Primary { dest } => {
            let file = locked
                .files
                .iter()
                .find(|file| file.primary)
                .ok_or_else(|| {
                    format!(
                        "no single primary file; choose with files = [{{ file_id = … }}] ({})",
                        describe(locked)
                    )
                })?;
            selected.push((place(dest, &file.path), file));
        }
        Select::Files(specs) => {
            for spec in specs {
                match &spec.what {
                    What::Path(path) => {
                        let file = locked
                            .files
                            .iter()
                            .find(|file| &file.path == path)
                            .ok_or_else(|| format!("no file {path:?}"))?;
                        selected.push((place(&spec.dest, &file.path), file));
                    }
                    What::Glob(pattern) => {
                        let glob = GlobBuilder::new(pattern)
                            .literal_separator(true)
                            .build()
                            .map_err(|e| e.to_string())?
                            .compile_matcher();
                        let before = selected.len();
                        for file in &locked.files {
                            if glob.is_match(&file.path) {
                                selected.push((join(&spec.dest, file_name(&file.path)), file));
                            }
                        }
                        if selected.len() == before {
                            return Err(format!("no file matches {pattern:?}"));
                        }
                    }
                    What::FileId(id) => {
                        let file = locked
                            .files
                            .iter()
                            .find(|file| file.file_id == Some(*id))
                            .ok_or_else(|| {
                                format!("no file with id {id} ({})", describe(locked))
                            })?;
                        selected.push((place(&spec.dest, &file.path), file));
                    }
                }
            }
        }
    }
    Ok(selected)
}

/// The destination of a single file: below `dest` under its own name if
/// `dest` is a directory (ends in `/`), else `dest` itself.
fn place(dest: &str, path: &str) -> String {
    if dest.ends_with('/') {
        join(dest, file_name(path))
    } else {
        dest.to_owned()
    }
}

fn join(dir: &str, path: &str) -> String {
    format!("{}/{path}", dir.trim_end_matches('/'))
}

fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// The files of a model version, for an error message.
fn describe(locked: &LockedModel) -> String {
    locked
        .files
        .iter()
        .map(|file| format!("{}: {}", file.file_id.unwrap_or(0), file.path))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use super::*;
    use crate::node::manifest::MANIFEST_NAME;
    use crate::source::Defaults;

    const REPO: &str = "hf:huggingface.co:example-org/example-model";
    const SITE: &str = "civitai:example.com:12345@67890";

    fn manifest(text: &str) -> Manifest {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(MANIFEST_NAME), text).unwrap();
        Manifest::load(dir.path(), &Defaults::default()).unwrap()
    }

    fn lock() -> Lock {
        let file = |path: &str, file_id: Option<u64>, primary: bool| LockedFile {
            path: path.into(),
            file_id,
            size: Some(path.len() as u64),
            sha256: Some(Sha256::of(path.as_bytes())),
            primary,
        };
        let mut lock = Lock::default();
        lock.insert(LockedModel {
            source: REPO.into(),
            revision: Some("main".into()),
            resolved: "0123abc".into(),
            files: [
                "README.md",
                "assets/sample.mp4",
                "split_files/upscaler/2x_a.pth",
                "split_files/upscaler/4x_a.pth",
                "split_files/upscaler/4x_b.pth",
                "split_files/vae/x_vae.safetensors",
            ]
            .iter()
            .map(|path| file(path, None, false))
            .collect(),
        });
        lock.insert(LockedModel {
            source: SITE.into(),
            revision: None,
            resolved: "67890".into(),
            files: vec![
                file("model.safetensors", Some(111), true),
                file("model.safetensors", Some(222), false),
            ],
        });
        lock
    }

    fn dests(text: &str) -> Vec<(String, String)> {
        plan(&manifest(text), &lock())
            .unwrap()
            .into_iter()
            .map(|p| (p.dest, p.path))
            .collect()
    }

    fn pair(dest: &str, path: &str) -> (String, String) {
        (dest.to_owned(), path.to_owned())
    }

    #[test]
    fn a_whole_repository_keeps_its_tree() {
        let placed = dests(
            "[[model]]\nsource = \"hf:example-org/example-model\"\ndest = \"models/x\"\nexclude = [\"assets\", \"*.pth\"]\n",
        );
        assert_eq!(
            placed,
            [
                pair("models/x/README.md", "README.md"),
                pair(
                    "models/x/split_files/vae/x_vae.safetensors",
                    "split_files/vae/x_vae.safetensors"
                ),
            ]
        );
    }

    #[test]
    fn listed_files_go_to_a_directory_or_a_name() {
        let placed = dests(
            r#"[[model]]
            source = "hf:example-org/example-model"
            files = [
              { path = "split_files/vae/x_vae.safetensors", dest = "models/vae/" },
              { path = "README.md", dest = "docs/model-readme.md" },
              { path = "split_files/upscaler/4x_*", dest = "models/upscale_models" },
            ]"#,
        );
        assert_eq!(
            placed,
            [
                pair("docs/model-readme.md", "README.md"),
                pair(
                    "models/upscale_models/4x_a.pth",
                    "split_files/upscaler/4x_a.pth"
                ),
                pair(
                    "models/upscale_models/4x_b.pth",
                    "split_files/upscaler/4x_b.pth"
                ),
                pair(
                    "models/vae/x_vae.safetensors",
                    "split_files/vae/x_vae.safetensors"
                ),
            ]
        );
    }

    #[test]
    fn a_model_version_places_its_primary_file_or_chosen_ones() {
        let primary = plan(
            &manifest(&format!(
                "[[model]]\nsource = \"{SITE}\"\ndest = \"models/loras/\"\n"
            )),
            &lock(),
        )
        .unwrap();
        assert_eq!(primary.len(), 1);
        assert_eq!(primary[0].dest, "models/loras/model.safetensors");
        assert_eq!(primary[0].file_id, Some(111));
        assert_eq!(primary[0].sha256, Some(Sha256::of(b"model.safetensors")));
        assert_eq!(primary[0].source, SITE);

        let chosen = plan(
            &manifest(&format!(
                "[[model]]\nsource = \"{SITE}\"\nfiles = [{{ file_id = 222, dest = \"models/loras/pruned.safetensors\" }}]\n"
            )),
            &lock(),
        )
        .unwrap();
        assert_eq!(chosen[0].dest, "models/loras/pruned.safetensors");
        assert_eq!(chosen[0].file_id, Some(222));
    }

    #[test]
    fn entries_of_the_same_model_are_combined() {
        let placed = dests(
            r#"[[model]]
            source = "hf:example-org/example-model"
            files = [{ path = "README.md", dest = "a/" }]

            [[model]]
            source = "hf:example-org/example-model"
            files = [{ path = "README.md", dest = "a/" }, { path = "README.md", dest = "b/" }]
            "#,
        );
        assert_eq!(
            placed,
            [
                pair("a/README.md", "README.md"),
                pair("b/README.md", "README.md")
            ]
        );
    }

    #[test]
    fn plans_that_cannot_be_carried_out_are_rejected() {
        let hf = "[[model]]\nsource = \"hf:example-org/example-model\"\n";
        for (text, expected) in [
            (
                format!("{hf}revision = \"dev\"\ndest = \"a/\"\n"),
                "is not in the lock file",
            ),
            (
                format!("{hf}files = [{{ path = \"missing.bin\", dest = \"a/\" }}]\n"),
                "no file \"missing.bin\"",
            ),
            (
                format!("{hf}files = [{{ path = \"*.zip\", dest = \"a/\" }}]\n"),
                "no file matches",
            ),
            (
                format!(
                    "{hf}files = [{{ path = \"README.md\", dest = \"a/x\" }}, {{ path = \"assets/sample.mp4\", dest = \"a/x\" }}]\n"
                ),
                "a/x is given twice",
            ),
            (
                format!(
                    "{hf}files = [{{ path = \"README.md\", dest = \"a/x\" }}, {{ path = \"assets/sample.mp4\", dest = \"a/x/\" }}]\n"
                ),
                "a/x is both a file and a directory (of a/x/sample.mp4)",
            ),
            (
                format!(
                    "[[model]]\nsource = \"{SITE}\"\nfiles = [{{ file_id = 999, dest = \"a/\" }}]\n"
                ),
                "no file with id 999 (111: model.safetensors",
            ),
        ] {
            let error = plan(&manifest(&text), &lock()).unwrap_err().to_string();
            assert!(error.contains(expected), "{text}\n=> {error}");
        }

        let mut ambiguous = lock();
        ambiguous.get_mut(SITE, None).unwrap().files[0].primary = false;
        let text = format!("[[model]]\nsource = \"{SITE}\"\ndest = \"a/\"\n");
        let error = plan(&manifest(&text), &ambiguous).unwrap_err().to_string();
        assert!(error.contains("no single primary file"), "{error}");
        assert!(error.starts_with("mvault.toml: model"), "{error}");
    }

    #[test]
    fn unresolved_models_are_listed_once() {
        let manifest = manifest(
            r#"[[model]]
            source = "hf:example-org/example-model"
            dest = "a/"

            [[model]]
            source = "hf:example-org/other"
            dest = "b/"

            [[model]]
            source = "hf:example-org/other"
            files = [{ path = "x", dest = "c/" }]

            [[model]]
            source = "hf:example-org/other"
            revision = "dev"
            dest = "d/"
            "#,
        );
        let missing: Vec<_> = unresolved(&manifest, &lock())
            .into_iter()
            .map(|model| (model.source.to_string(), model.revision.clone()))
            .collect();
        assert_eq!(
            missing,
            [
                (
                    "hf:huggingface.co:example-org/other".to_owned(),
                    Some("main".to_owned())
                ),
                (
                    "hf:huggingface.co:example-org/other".to_owned(),
                    Some("dev".to_owned())
                ),
            ]
        );
    }
}
