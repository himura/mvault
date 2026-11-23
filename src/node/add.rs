//! Appending a model to the manifest.

use std::fs;
use std::io;

use super::lock::Lock;
use super::manifest::{self, DEFAULT_REVISION, MANIFEST_NAME, Manifest, Model};
use super::plan::plan;
use super::resolve::{Access, resolve};
use super::workspace::Workspace;
use crate::error::{Error, Result};
use crate::fsutil::{check_relative, io_error, write_atomic};
use crate::source::{DEFAULT_HF_HOST, Reference, Source};

/// What to add.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// The model, and what a URL said about revision and file.
    pub reference: &'a Reference,
    /// Where to place it: a directory (ending in `/`) or a file name.
    pub dest: &'a str,
    /// Paths or globs of files of a repository, instead of all of it.
    pub paths: &'a [String],
    /// Ids of files of a model version, instead of the primary one.
    pub file_ids: &'a [u64],
    /// Branch, tag or commit of a repository.
    pub revision: Option<&'a str>,
    /// The manifest file to append to, relative to the workspace.
    pub file: &'a str,
}

/// What adding did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Added {
    /// The entry was appended.
    Appended {
        /// The text of the entry.
        entry: String,
        /// The commit or version the model is resolved to.
        resolved: String,
    },
    /// The manifest already has this entry; nothing was written.
    AlreadyThere,
}

/// Appends an entry for a model to a manifest file and resolves the model
/// into the lock file. No model file is fetched.
///
/// The file is appended to as text, so that comments and layout stay as the
/// user wrote them.
///
/// # Errors
///
/// Returns an error, writing nothing, if the request does not fit the kind
/// of source, the model cannot be resolved or does not have the files
/// asked for, a destination is already used by another file, or the
/// manifest file is not one the workspace reads.
pub fn add(workspace: &Workspace, access: &Access<'_>, request: &Request<'_>) -> Result<Added> {
    let defaults = workspace.defaults();
    let mut manifest = Manifest::load(&workspace.root, &defaults)?;
    check_relative(request.file).map_err(Error::Invalid)?;
    let entry = entry_text(request)?;
    let model = manifest::parse_text(&entry, request.file, &defaults)?
        .pop()
        .ok_or_else(|| Error::Invalid("the entry describes no model".into()))?;
    if manifest
        .models
        .iter()
        .any(|other| same_entry(other, &model))
    {
        return Ok(Added::AlreadyThere);
    }

    let lock_path = workspace.lock_path();
    let mut lock = Lock::load(&lock_path)?;
    let (source, revision) = model.key();
    let mut lock_changed = false;
    if lock.get(&source, revision).is_none() {
        lock.insert(resolve(&model, access)?);
        lock_changed = true;
    }
    let resolved = lock
        .get(&source, revision)
        .map(|locked| locked.resolved.clone())
        .unwrap_or_default();
    // The model must have what the entry asks for, and the destinations
    // must be free, before anything is written.
    manifest.models.push(model.clone());
    plan(&manifest, &lock)?;

    let path = workspace.root.join(request.file);
    let before = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_error(&path)(error)),
    };
    let mut text = before.clone().unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&entry);
    write_atomic(&path, text.as_bytes())?;

    // Read back the way a sync will: the file may fail to parse with the
    // entry appended, or not be included by the manifest at all.
    let read_back = Manifest::load(&workspace.root, &defaults).and_then(|manifest| {
        if manifest
            .models
            .iter()
            .any(|other| same_entry(other, &model))
        {
            Ok(())
        } else {
            Err(Error::Invalid(format!(
                "{} is not included by {MANIFEST_NAME}",
                request.file
            )))
        }
    });
    if let Err(error) = read_back {
        match before {
            Some(text) => write_atomic(&path, text.as_bytes())?,
            None => fs::remove_file(&path).map_err(io_error(&path))?,
        }
        return Err(error);
    }
    if lock_changed {
        lock.save(&lock_path)?;
    }
    Ok(Added::Appended { entry, resolved })
}

fn same_entry(a: &Model, b: &Model) -> bool {
    a.key() == b.key() && a.select == b.select
}

/// The `[[model]]` entry for a request, as TOML text.
fn entry_text(request: &Request<'_>) -> Result<String> {
    let reference = request.reference;
    let dest = quote(request.dest);
    let mut lines = vec!["[[model]]".to_owned()];
    let files: Vec<String> = match &reference.source {
        Source::HuggingFace { host, repo } => {
            if !request.file_ids.is_empty() {
                return Err(Error::Invalid(
                    "--file-id is for model versions; files of a repository are given with --path"
                        .into(),
                ));
            }
            let source = if host == DEFAULT_HF_HOST {
                format!("hf:{repo}")
            } else {
                reference.source.to_string()
            };
            lines.push(format!("source = {}", quote(&source)));
            let revision = match (request.revision, reference.revision.as_deref()) {
                (Some(given), Some(in_url)) if given != in_url => {
                    return Err(Error::Invalid(format!(
                        "--revision {given} contradicts the revision {in_url} in the URL"
                    )));
                }
                (Some(revision), _) | (None, Some(revision)) => revision,
                (None, None) => DEFAULT_REVISION,
            };
            if revision != DEFAULT_REVISION {
                lines.push(format!("revision = {}", quote(revision)));
            }
            reference
                .path
                .iter()
                .chain(request.paths)
                .map(|path| format!("  {{ path = {}, dest = {dest} }},", quote(path)))
                .collect()
        }
        Source::Civitai { .. } => {
            if !request.paths.is_empty() || request.revision.is_some() {
                return Err(Error::Invalid(
                    "--path and --revision are for repositories; files of a model version are given with --file-id"
                        .into(),
                ));
            }
            lines.push(format!("source = {}", quote(&reference.source.to_string())));
            reference
                .file_id
                .iter()
                .chain(request.file_ids)
                .map(|id| format!("  {{ file_id = {id}, dest = {dest} }},"))
                .collect()
        }
    };
    if files.is_empty() {
        lines.push(format!("dest = {dest}"));
    } else {
        lines.push("files = [".to_owned());
        lines.extend(files);
        lines.push("]".to_owned());
    }
    Ok(lines.join("\n") + "\n")
}

/// A TOML string for `value`.
fn quote(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::download::{Client, RetryPolicy};
    use crate::hash::Sha256;
    use crate::node::manifest::LOCK_NAME;
    use crate::source::Defaults;
    use crate::testutil::Routes;
    use crate::upstream::civitai::Site;
    use crate::upstream::hf::Hub;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    /// What a test passes to `add` beyond the source and the destination.
    #[derive(Default, Clone, Copy)]
    struct Extra<'a> {
        paths: &'a [String],
        file_ids: &'a [u64],
        revision: Option<&'a str>,
        file: Option<&'a str>,
    }

    fn none() -> Extra<'static> {
        Extra::default()
    }

    /// An upstream with a repository on the default hub and a model; no
    /// mirror, so that every resolution asks the upstream.
    struct World {
        dir: tempfile::TempDir,
        upstream: Routes,
    }

    impl World {
        fn new() -> Self {
            let upstream = Routes::start();
            let base = upstream.base();
            for revision in ["main", "dev"] {
                upstream.route(
                    &format!(
                        "/api/models/example-org/example-model/revision/{revision}?blobs=true"
                    ),
                    format!(
                        r#"{{"sha": "{COMMIT}", "siblings": [
                            {{"rfilename": "config.json", "size": 2}},
                            {{"rfilename": "vae/a.safetensors", "size": 1}},
                            {{"rfilename": "vae/b.safetensors", "size": 1}}
                        ]}}"#
                    )
                    .as_bytes(),
                );
            }
            upstream.route(
                "/api/v1/models/12345",
                br#"{"id": 12345, "modelVersions": [{"id": 67890}]}"#,
            );
            upstream.route(
                "/api/v1/model-versions/67890",
                format!(
                    r#"{{"id": 67890, "modelId": 12345, "files": [
                        {{"id": 111, "name": "lora.safetensors", "primary": true,
                          "hashes": {{"SHA256": "{}"}}, "downloadUrl": "{base}/download/111"}},
                        {{"id": 222, "name": "extra.zip", "downloadUrl": "{base}/download/222"}}
                    ]}}"#,
                    Sha256::of(b"lora")
                )
                .as_bytes(),
            );
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(MANIFEST_NAME), "# my models\n").unwrap();
            Self { dir, upstream }
        }

        fn add(&self, source: &str, dest: &str, extra: Extra<'_>) -> Result<Added> {
            let workspace = Workspace::open(self.dir.path().to_owned(), None).unwrap();
            let reference = Reference::parse(source, &Defaults::default()).unwrap();
            let base = self.upstream.base();
            let http = Client::new(RetryPolicy {
                max_failures: 1,
                ..RetryPolicy::default()
            });
            let access = Access {
                http: &http,
                remotes: &[],
                hub: &|_| Hub::new(&base, None),
                site: &|_| Site::new(&base, None),
                warnings: RefCell::default(),
            };
            let request = Request {
                reference: &reference,
                dest,
                paths: extra.paths,
                file_ids: extra.file_ids,
                revision: extra.revision,
                file: extra.file.unwrap_or(MANIFEST_NAME),
            };
            add(&workspace, &access, &request)
        }

        fn manifest(&self) -> String {
            fs::read_to_string(self.dir.path().join(MANIFEST_NAME)).unwrap()
        }

        fn lock(&self) -> String {
            fs::read_to_string(self.dir.path().join(LOCK_NAME)).unwrap_or_default()
        }
    }

    #[test]
    fn a_repository_url_becomes_an_entry_for_the_whole_repository() {
        let world = World::new();

        let added = world
            .add(
                "https://huggingface.co/example-org/example-model",
                "models/x/",
                none(),
            )
            .unwrap();

        let entry = "[[model]]\nsource = \"hf:example-org/example-model\"\ndest = \"models/x/\"\n";
        assert_eq!(
            added,
            Added::Appended {
                entry: entry.to_owned(),
                resolved: COMMIT.to_owned()
            }
        );
        assert_eq!(world.manifest(), format!("# my models\n\n{entry}"));
        assert!(world.lock().contains(&format!("resolved = \"{COMMIT}\"")));
        assert!(world.lock().contains("vae/b.safetensors"));
    }

    #[test]
    fn a_file_url_and_paths_become_a_list_of_files() {
        let world = World::new();
        let paths = ["vae/*.safetensors".to_owned()];

        world
            .add(
                "https://huggingface.co/example-org/example-model/blob/dev/config.json",
                "models/x/",
                Extra {
                    paths: &paths,
                    ..none()
                },
            )
            .unwrap();

        assert_eq!(
            world.manifest(),
            "# my models\n\n[[model]]\nsource = \"hf:example-org/example-model\"\nrevision = \"dev\"\nfiles = [\n  \
             { path = \"config.json\", dest = \"models/x/\" },\n  \
             { path = \"vae/*.safetensors\", dest = \"models/x/\" },\n]\n"
        );
    }

    #[test]
    fn a_model_becomes_an_entry_for_its_primary_file_or_chosen_ones() {
        let world = World::new();

        world
            .add("civitai:example.com:12345", "models/loras/", none())
            .unwrap();
        let ids = [222];
        world
            .add(
                "https://example.com/models/12345?modelVersionId=67890",
                "extra/",
                Extra {
                    file_ids: &ids,
                    ..none()
                },
            )
            .unwrap();

        assert_eq!(
            world.manifest(),
            "# my models\n\n[[model]]\nsource = \"civitai:example.com:12345\"\ndest = \"models/loras/\"\n\n\
             [[model]]\nsource = \"civitai:example.com:12345@67890\"\nfiles = [\n  \
             { file_id = 222, dest = \"extra/\" },\n]\n"
        );
        // The latest version is pinned in the lock, under the source as written.
        let lock = Lock::load(&world.dir.path().join(LOCK_NAME)).unwrap();
        assert_eq!(
            lock.get("civitai:example.com:12345", None)
                .unwrap()
                .resolved,
            "67890"
        );
        assert_eq!(
            lock.get("civitai:example.com:12345@67890", None)
                .unwrap()
                .resolved,
            "67890"
        );
    }

    #[test]
    fn adding_the_same_entry_again_changes_nothing() {
        let world = World::new();
        world
            .add("hf:example-org/example-model", "models/x/", none())
            .unwrap();
        let (manifest, lock) = (world.manifest(), world.lock());
        let requests = world.upstream.server.requests().len();

        let again = world
            .add("hf:example-org/example-model", "models/x/", none())
            .unwrap();

        assert_eq!(again, Added::AlreadyThere);
        assert_eq!((world.manifest(), world.lock()), (manifest, lock));
        assert_eq!(world.upstream.server.requests().len(), requests);
    }

    #[test]
    fn a_model_in_the_lock_is_not_resolved_again() {
        let world = World::new();
        world
            .add("hf:example-org/example-model", "models/x/", none())
            .unwrap();
        let requests = world.upstream.server.requests().len();
        let paths = ["config.json".to_owned()];

        world
            .add(
                "hf:example-org/example-model",
                "other/",
                Extra {
                    paths: &paths,
                    ..none()
                },
            )
            .unwrap();

        assert_eq!(world.upstream.server.requests().len(), requests);
    }

    #[test]
    fn an_entry_can_go_to_an_included_file() {
        let world = World::new();
        fs::write(
            world.dir.path().join(MANIFEST_NAME),
            "include = [\"sets/*.toml\"]\n",
        )
        .unwrap();
        fs::create_dir_all(world.dir.path().join("sets")).unwrap();

        world
            .add(
                "hf:example-org/example-model",
                "models/x/",
                Extra {
                    file: Some("sets/new.toml"),
                    ..none()
                },
            )
            .unwrap();

        assert_eq!(
            fs::read_to_string(world.dir.path().join("sets/new.toml")).unwrap(),
            "[[model]]\nsource = \"hf:example-org/example-model\"\ndest = \"models/x/\"\n"
        );
        assert_eq!(world.manifest(), "include = [\"sets/*.toml\"]\n");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn nothing_is_written_when_the_entry_cannot_work() {
        let world = World::new();
        world
            .add("hf:example-org/example-model", "models/x/", none())
            .unwrap();
        let (manifest, lock) = (world.manifest(), world.lock());
        let missing = ["missing.bin".to_owned()];
        let config = ["config.json".to_owned()];
        let ids = [1];

        let cases: Vec<(Result<Added>, &str)> = vec![
            (
                world.add(
                    "hf:example-org/example-model",
                    "y/",
                    Extra {
                        paths: &missing,
                        ..none()
                    },
                ),
                "no file \"missing.bin\"",
            ),
            (
                world.add(
                    "hf:example-org/example-model",
                    "models/x/config.json",
                    none(),
                ),
                "both a file and a directory",
            ),
            (
                world.add("hf:example-org/unknown", "y/", none()),
                "HTTP status 404",
            ),
            (
                world.add("hf:example-org/example-model", "../y/", none()),
                "inside the workspace",
            ),
            (
                world.add(
                    "hf:example-org/example-model",
                    "y/",
                    Extra {
                        file_ids: &ids,
                        ..none()
                    },
                ),
                "--file-id is for model versions",
            ),
            (
                world.add(
                    "civitai:example.com:12345",
                    "y/",
                    Extra {
                        paths: &config,
                        ..none()
                    },
                ),
                "--path and --revision are for repositories",
            ),
            (
                world.add(
                    "civitai:example.com:12345",
                    "y/",
                    Extra {
                        revision: Some("main"),
                        ..none()
                    },
                ),
                "--path and --revision are for repositories",
            ),
            (
                world.add(
                    "https://huggingface.co/example-org/example-model/tree/dev",
                    "y/",
                    Extra {
                        revision: Some("main"),
                        ..none()
                    },
                ),
                "contradicts the revision dev",
            ),
            (
                world.add(
                    "hf:example-org/example-model",
                    "y/",
                    Extra {
                        file: Some("other.toml"),
                        ..none()
                    },
                ),
                "other.toml is not included by mvault.toml",
            ),
            (
                world.add(
                    "hf:example-org/example-model",
                    "y/",
                    Extra {
                        file: Some("../outside.toml"),
                        ..none()
                    },
                ),
                "unsafe path",
            ),
        ];
        for (result, expected) in cases {
            let error = result.unwrap_err().to_string();
            assert!(error.contains(expected), "{expected}: {error}");
        }
        assert_eq!((world.manifest(), world.lock()), (manifest, lock));
        assert!(!world.dir.path().join("other.toml").exists());
    }

    #[test]
    fn a_manifest_without_a_final_newline_is_completed() {
        let world = World::new();
        fs::write(world.dir.path().join(MANIFEST_NAME), "# no newline").unwrap();

        world
            .add("hf:example-org/example-model", "models/x/", none())
            .unwrap();

        assert!(world.manifest().starts_with("# no newline\n\n[[model]]\n"));
    }
}
