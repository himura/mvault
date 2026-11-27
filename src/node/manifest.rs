//! The manifest: which models a workspace wants, and where.
//!
//! `mvault.toml` at the root of the workspace may include further files, so
//! that models can be grouped. Only people write these files.

use std::fs;
use std::path::Path;

use globset::GlobBuilder;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::fsutil::{check_relative, io_error, is_git_dir};
use crate::mirror::exclude::Excludes;
use crate::source::{Defaults, Reference, Source};

/// Name of the manifest at the root of a workspace.
pub const MANIFEST_NAME: &str = "mvault.toml";
/// Name of the lock file at the root of a workspace.
pub const LOCK_NAME: &str = "mvault.lock";
/// Name of the directory for what belongs to one node.
pub const STATE_DIR: &str = ".mvault";

/// The revision of a repository when the manifest names none.
pub const DEFAULT_REVISION: &str = "main";

/// Every model of a workspace, in the order written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The models.
    pub models: Vec<Model>,
    /// The files of the manifest, relative to the workspace: `mvault.toml`
    /// and the files it includes.
    pub files: Vec<String>,
}

/// One `[[model]]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// Where the model comes from.
    pub source: Source,
    /// Branch, tag or commit of a repository; `None` for a model version.
    pub revision: Option<String>,
    /// Which files to place, and where.
    pub select: Select,
    /// The manifest file the entry is written in, relative to the workspace.
    pub file: String,
}

/// Which files of a model to place, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Select {
    /// The whole repository below `dest`, keeping its tree.
    Whole {
        /// Directory to place the tree in.
        dest: String,
        /// Globs of files to leave out.
        exclude: Vec<String>,
        /// Regular expressions of files to leave out.
        exclude_regexp: Vec<String>,
    },
    /// The primary file of a model version.
    Primary {
        /// Directory (ending in `/`) or file name to place it at.
        dest: String,
    },
    /// The listed files.
    Files(Vec<FileSpec>),
}

/// One entry of `files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec {
    /// Which file or files.
    pub what: What,
    /// Directory (ending in `/`) or file name to place at. Always a
    /// directory for a glob.
    pub dest: String,
}

/// What an entry of `files` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum What {
    /// A path in a repository, as it is: `*` or `[` in it are part of the
    /// name.
    Path(String),
    /// A glob over the paths of a repository. `*` does not cross a `/`;
    /// `**` does.
    Glob(String),
    /// The id of a file of a model version.
    FileId(u64),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    model: Vec<RawModel>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    source: String,
    revision: Option<String>,
    dest: Option<String>,
    exclude: Option<Vec<String>>,
    exclude_regexp: Option<Vec<String>>,
    files: Option<Vec<RawFile>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    path: Option<String>,
    glob: Option<String>,
    file_id: Option<u64>,
    dest: String,
}

impl Model {
    /// What the lock file knows this model by: the canonical source and the
    /// revision as written.
    #[must_use]
    pub fn key(&self) -> (String, Option<&str>) {
        (self.source.to_string(), self.revision.as_deref())
    }
}

impl Manifest {
    /// Reads `mvault.toml` in `workspace` and the files it includes.
    ///
    /// # Errors
    ///
    /// Returns an error if a file cannot be read, is not valid, or an entry
    /// does not make sense. The message names the file.
    pub fn load(workspace: &Path, defaults: &Defaults) -> Result<Self> {
        let root = read(workspace, MANIFEST_NAME)?;
        let mut models = parse_models(root.model, MANIFEST_NAME, defaults)?;
        let mut files = vec![MANIFEST_NAME.to_owned()];
        for file in included_files(workspace, &root.include)? {
            let included = read(workspace, &file)?;
            if !included.include.is_empty() {
                return Err(Error::Invalid(format!(
                    "{file}: only {MANIFEST_NAME} may include other files"
                )));
            }
            models.extend(parse_models(included.model, &file, defaults)?);
            files.push(file);
        }
        Ok(Self { models, files })
    }
}

/// Whether an `include` pattern is a glob rather than a plain path. Only
/// for `include`, which names the files of the workspace itself; a file of
/// a model is given by `path` or by `glob`.
fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', '{'])
}

fn read(workspace: &Path, file: &str) -> Result<RawManifest> {
    let path = workspace.join(file);
    let text = fs::read_to_string(&path).map_err(io_error(&path))?;
    toml::from_str(&text).map_err(|e| Error::Invalid(format!("{file}: {e}")))
}

/// The models written in `text`, the content of the manifest file `file`.
pub(crate) fn parse_text(text: &str, file: &str, defaults: &Defaults) -> Result<Vec<Model>> {
    let raw: RawManifest =
        toml::from_str(text).map_err(|e| Error::Invalid(format!("{file}: {e}")))?;
    parse_models(raw.model, file, defaults)
}

/// The files `include` names, in the order written; the matches of a glob
/// in the order of their names.
fn included_files(workspace: &Path, patterns: &[String]) -> Result<Vec<String>> {
    let mut files: Vec<String> = Vec::new();
    for pattern in patterns {
        let invalid = |reason: String| Error::Invalid(format!("include {pattern:?}: {reason}"));
        check_relative(pattern).map_err(invalid)?;
        if !is_glob(pattern) {
            files.push(pattern.clone());
            continue;
        }
        let (dir, name) = pattern.rsplit_once('/').unwrap_or(("", pattern));
        if is_glob(dir) {
            return Err(invalid("a glob is only allowed in the file name".into()));
        }
        let matcher = GlobBuilder::new(name)
            .literal_separator(true)
            .build()
            .map_err(|e| invalid(e.to_string()))?
            .compile_matcher();
        let dir_path = workspace.join(dir);
        let entries = match fs::read_dir(&dir_path) {
            Ok(entries) => entries,
            // No directory, no matches: as with a directory without them.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(&dir_path)(error)),
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_error(&dir_path))?;
            let Ok(file_name) = entry.file_name().into_string() else {
                continue;
            };
            let is_file = entry.file_type().is_ok_and(|kind| !kind.is_dir());
            if is_file && matcher.is_match(&file_name) {
                found.push(if dir.is_empty() {
                    file_name
                } else {
                    format!("{dir}/{file_name}")
                });
            }
        }
        found.sort();
        files.extend(found);
    }
    // The root manifest is not its own include, and no file counts twice.
    let mut seen = vec![MANIFEST_NAME.to_owned()];
    files.retain(|file| {
        let new = !seen.contains(file);
        if new {
            seen.push(file.clone());
        }
        new
    });
    Ok(files)
}

fn parse_models(raw: Vec<RawModel>, file: &str, defaults: &Defaults) -> Result<Vec<Model>> {
    raw.into_iter()
        .map(|model| {
            let name = model.source.clone();
            parse_model(model, defaults)
                .map(|(source, revision, select)| Model {
                    source,
                    revision,
                    select,
                    file: file.to_owned(),
                })
                .map_err(|reason| Error::Invalid(format!("{file}: model {name:?}: {reason}")))
        })
        .collect()
}

type Parsed = (Source, Option<String>, Select);

fn parse_model(raw: RawModel, defaults: &Defaults) -> std::result::Result<Parsed, String> {
    if !raw.source.starts_with("hf:") && !raw.source.starts_with("civitai:") {
        return Err("source must be written as hf:… or civitai:… (mvault add accepts URLs)".into());
    }
    let reference = Reference::parse(&raw.source, defaults).map_err(|e| e.to_string())?;
    let source = reference.source;
    let is_repo = matches!(source, Source::HuggingFace { .. });

    let revision = match (is_repo, raw.revision) {
        (true, revision) => {
            let revision = revision.unwrap_or_else(|| DEFAULT_REVISION.to_owned());
            if revision.is_empty() {
                return Err("revision is empty".into());
            }
            Some(revision)
        }
        (false, None) => None,
        (false, Some(_)) => {
            return Err(
                "revision is for repositories; a model version is part of the source".into(),
            );
        }
    };

    let has_excludes = raw.exclude.is_some() || raw.exclude_regexp.is_some();
    let select = match (raw.dest, raw.files) {
        (Some(_), Some(_)) => return Err("give either dest or files, not both".into()),
        (None, None) => return Err("needs dest or files".into()),
        (Some(dest), None) => {
            check_dest(&dest)?;
            if is_repo {
                let exclude = raw.exclude.unwrap_or_default();
                let exclude_regexp = raw.exclude_regexp.unwrap_or_default();
                Excludes::new(&exclude, &exclude_regexp).map_err(|e| e.to_string())?;
                Select::Whole {
                    dest,
                    exclude,
                    exclude_regexp,
                }
            } else if has_excludes {
                return Err("exclude and exclude_regexp are for whole repositories".into());
            } else {
                Select::Primary { dest }
            }
        }
        (None, Some(files)) => {
            if has_excludes {
                return Err(
                    "exclude and exclude_regexp are for whole repositories, not for files".into(),
                );
            }
            if files.is_empty() {
                return Err("files is empty".into());
            }
            let specs = files
                .into_iter()
                .map(|file| parse_file(file, is_repo))
                .collect::<std::result::Result<_, _>>()?;
            Select::Files(specs)
        }
    };
    Ok((source, revision, select))
}

fn parse_file(raw: RawFile, is_repo: bool) -> std::result::Result<FileSpec, String> {
    check_dest(&raw.dest)?;
    // The kind of an entry is told by its key, never by how it is written,
    // so that a file whose name has `*` or `[` in it can be chosen.
    let what = match (is_repo, raw.path, raw.glob, raw.file_id) {
        (true, Some(path), None, None) => {
            check_relative(&path)?;
            What::Path(path)
        }
        (true, None, Some(glob), None) => {
            GlobBuilder::new(&glob)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("invalid glob {glob:?}: {e}"))?;
            What::Glob(glob)
        }
        (false, None, None, Some(file_id)) => What::FileId(file_id),
        (true, _, _, _) => {
            return Err("a file of a repository is given by one of path or glob".into());
        }
        (false, _, _, _) => return Err("a file of a model version is given by file_id".into()),
    };
    Ok(FileSpec {
        what,
        dest: raw.dest,
    })
}

/// Checks that `dest` stays inside the workspace and away from what mvault
/// itself keeps there.
pub(crate) fn check_dest(dest: &str) -> std::result::Result<(), String> {
    let path = dest.strip_suffix('/').unwrap_or(dest);
    if path.split('/').any(is_git_dir) {
        return Err(format!("dest {dest:?} leads into a git directory"));
    }
    check_relative(path)
        .map_err(|_| format!("dest {dest:?} must be a path inside the workspace"))?;
    let reserved =
        path.split('/').next() == Some(STATE_DIR) || [MANIFEST_NAME, LOCK_NAME].contains(&path);
    if reserved {
        return Err(format!("dest {dest:?} is reserved for mvault"));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn workspace(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        dir
    }

    fn load(files: &[(&str, &str)]) -> Result<Manifest> {
        Manifest::load(workspace(files).path(), &Defaults::default())
    }

    fn error_of(manifest: &str) -> String {
        load(&[(MANIFEST_NAME, manifest)]).unwrap_err().to_string()
    }

    #[test]
    fn an_empty_manifest_has_no_models() {
        assert_eq!(load(&[(MANIFEST_NAME, "")]).unwrap().models, []);
    }

    #[test]
    fn reads_every_kind_of_entry() {
        let manifest = load(&[(
            MANIFEST_NAME,
            r#"
            [[model]]
            source = "hf:example-org/example-repackaged"
            files = [
              { path = "split_files/vae/x_vae.safetensors", dest = "models/vae/" },
              { glob = "split_files/upscaler/4x_*", dest = "models/upscale_models/" },
              { path = "odd/[name]*.bin", dest = "models/odd/" },
            ]

            [[model]]
            source = "hf:hub.example.com:example-org/example-diffusers"
            revision = "v1.0"
            dest = "models/diffusers/example/"
            exclude = ["assets"]
            exclude_regexp = ['\.mp4$']

            [[model]]
            source = "civitai:example.com:12345@67890"
            dest = "models/loras/"

            [[model]]
            source = "civitai:example.com:23456"
            files = [{ file_id = 111, dest = "models/vae/renamed.safetensors" }]
            "#,
        )])
        .unwrap();

        let [repackaged, diffusers, pinned, latest] = manifest.models.as_slice() else {
            panic!("{manifest:?}");
        };
        assert_eq!(
            repackaged.key(),
            (
                "hf:huggingface.co:example-org/example-repackaged".into(),
                Some("main")
            )
        );
        assert_eq!(
            repackaged.select,
            Select::Files(vec![
                FileSpec {
                    what: What::Path("split_files/vae/x_vae.safetensors".into()),
                    dest: "models/vae/".into()
                },
                FileSpec {
                    what: What::Glob("split_files/upscaler/4x_*".into()),
                    dest: "models/upscale_models/".into()
                },
                FileSpec {
                    what: What::Path("odd/[name]*.bin".into()),
                    dest: "models/odd/".into()
                },
            ])
        );
        assert_eq!(
            diffusers.key(),
            (
                "hf:hub.example.com:example-org/example-diffusers".into(),
                Some("v1.0")
            )
        );
        assert_eq!(
            diffusers.select,
            Select::Whole {
                dest: "models/diffusers/example/".into(),
                exclude: vec!["assets".into()],
                exclude_regexp: vec![r"\.mp4$".into()],
            }
        );
        assert_eq!(
            pinned.key(),
            ("civitai:example.com:12345@67890".into(), None)
        );
        assert_eq!(
            pinned.select,
            Select::Primary {
                dest: "models/loras/".into()
            }
        );
        assert_eq!(latest.key(), ("civitai:example.com:23456".into(), None));
        assert_eq!(
            latest.select,
            Select::Files(vec![FileSpec {
                what: What::FileId(111),
                dest: "models/vae/renamed.safetensors".into()
            }])
        );
        assert!(
            manifest
                .models
                .iter()
                .all(|model| model.file == MANIFEST_NAME)
        );
    }

    #[test]
    fn the_default_site_host_applies() {
        let dir = workspace(&[(
            MANIFEST_NAME,
            "[[model]]\nsource = \"civitai:12345\"\ndest = \"loras/\"\n",
        )]);
        let defaults = Defaults {
            civitai_host: Some("example.com".into()),
        };
        let manifest = Manifest::load(dir.path(), &defaults).unwrap();
        assert_eq!(manifest.models[0].key().0, "civitai:example.com:12345");
        assert!(Manifest::load(dir.path(), &Defaults::default()).is_err());
    }

    #[test]
    fn included_files_follow_the_root_in_order() {
        let entry = |repo: &str| {
            format!("[[model]]\nsource = \"hf:example-org/{repo}\"\ndest = \"{repo}/\"\n")
        };
        let manifest = load(&[
            (
                MANIFEST_NAME,
                &format!(
                    "include = [\"sets/*.toml\", \"extra.toml\", \"sets/b.toml\"]\n{}",
                    entry("root")
                ),
            ),
            ("sets/b.toml", &entry("b")),
            ("sets/a.toml", &entry("a")),
            ("sets/notes.txt", "not a manifest"),
            ("extra.toml", &entry("extra")),
        ])
        .unwrap();

        let files: Vec<&str> = manifest
            .models
            .iter()
            .map(|model| model.file.as_str())
            .collect();
        assert_eq!(
            files,
            [MANIFEST_NAME, "sets/a.toml", "sets/b.toml", "extra.toml"]
        );
    }

    #[test]
    fn includes_are_checked() {
        let nested = load(&[
            (MANIFEST_NAME, "include = [\"a.toml\"]\n"),
            ("a.toml", "include = [\"b.toml\"]\n"),
        ]);
        assert!(
            nested
                .unwrap_err()
                .to_string()
                .contains("only mvault.toml may include")
        );

        for include in ["../outside.toml", "/abs.toml", "*/a.toml", "missing.toml"] {
            let result = load(&[(MANIFEST_NAME, &format!("include = [\"{include}\"]\n"))]);
            assert!(result.is_err(), "{include}");
        }
        // A glob that matches nothing includes nothing, also in a directory
        // that does not exist.
        for include in ["*.none", "missing/*.toml"] {
            assert_eq!(
                load(&[(MANIFEST_NAME, &format!("include = [\"{include}\"]\n"))])
                    .unwrap()
                    .models,
                []
            );
        }
    }

    #[test]
    fn errors_name_the_file_and_the_model() {
        let result = load(&[
            (MANIFEST_NAME, "include = [\"a.toml\"]\n"),
            (
                "a.toml",
                "[[model]]\nsource = \"hf:example-org/example-model\"\n",
            ),
        ]);
        assert_eq!(
            result.unwrap_err().to_string(),
            "a.toml: model \"hf:example-org/example-model\": needs dest or files"
        );
    }

    #[test]
    fn entries_that_make_no_sense_are_rejected() {
        let hf = "[[model]]\nsource = \"hf:example-org/example-model\"\n";
        let civitai = "[[model]]\nsource = \"civitai:example.com:12345@67890\"\n";
        for (manifest, expected) in [
            ("unknown = 1\n".to_owned(), "unknown field"),
            (format!("{hf}dest = \"a/\"\nextra = 1\n"), "unknown field"),
            (
                "[[model]]\nsource = \"https://huggingface.co/example-org/example-model\"\ndest = \"a/\"\n".to_owned(),
                "source must be written as",
            ),
            ("[[model]]\nsource = \"hf:a/b/c\"\ndest = \"a/\"\n".to_owned(), "invalid source"),
            (format!("{hf}dest = \"a/\"\nfiles = [{{ path = \"x\", dest = \"b/\" }}]\n"), "either dest or files"),
            (format!("{hf}files = []\n"), "files is empty"),
            (format!("{hf}revision = \"\"\ndest = \"a/\"\n"), "revision is empty"),
            (format!("{hf}files = [{{ file_id = 1, dest = \"b/\" }}]\n"), "one of path or glob"),
            (format!("{hf}files = [{{ path = \"x\", file_id = 1, dest = \"b/\" }}]\n"), "one of path or glob"),
            (format!("{hf}files = [{{ path = \"x\", glob = \"x\", dest = \"b/\" }}]\n"), "one of path or glob"),
            (format!("{hf}files = [{{ path = \"../x\", dest = \"b/\" }}]\n"), "unsafe path"),
            (format!("{hf}files = [{{ glob = \"a[\", dest = \"b/\" }}]\n"), "invalid glob"),
            (format!("{hf}dest = \"a/\"\nexclude_regexp = [\"(\"]\n"), "invalid exclude regular expression"),
            (format!("{civitai}files = [{{ glob = \"x\", dest = \"b/\" }}]\n"), "given by file_id"),
            (format!("{hf}files = [{{ path = \"x\", dest = \"b/\" }}]\nexclude = [\"y\"]\n"), "for whole repositories"),
            (format!("{hf}files = [{{ path = \"x\", dest = \"b/\" }}]\nexclude_regexp = [\"y\"]\n"), "for whole repositories"),
            (format!("{hf}files = [{{ path = \"x\" }}]\n"), "missing field `dest`"),
            (format!("{civitai}revision = \"main\"\ndest = \"a/\"\n"), "revision is for repositories"),
            (format!("{civitai}dest = \"a/\"\nexclude = [\"y\"]\n"), "for whole repositories"),
            (format!("{civitai}files = [{{ path = \"x\", dest = \"b/\" }}]\n"), "given by file_id"),
            (format!("{hf}dest = \"../outside/\"\n"), "inside the workspace"),
            (format!("{hf}dest = \"/abs/\"\n"), "inside the workspace"),
            (format!("{hf}dest = \"\"\n"), "inside the workspace"),
            (format!("{hf}dest = \".mvault/blobs/\"\n"), "reserved"),
            (format!("{civitai}dest = \"mvault.lock\"\n"), "reserved"),
            (format!("{hf}files = [{{ path = \"x\", dest = \"mvault.toml\" }}]\n"), "reserved"),
            (format!("{hf}dest = \".git/\"\n"), "git directory"),
            (format!("{hf}files = [{{ path = \"x\", dest = \"models/.Git/config\" }}]\n"), "git directory"),
            (format!("{hf}files = [{{ path = \".git/config\", dest = \"b/\" }}]\n"), "unsafe path"),
        ] {
            let error = error_of(&manifest);
            assert!(error.contains(expected), "{manifest}\n=> {error}");
        }
    }
}
