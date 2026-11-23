//! The lock file: what each model of the manifest resolved to.
//!
//! `mvault.lock` is the one file mvault writes into a workspace's version
//! control. For every source and revision of the manifest it holds the
//! commit or version that was resolved and every file the upstream lists
//! there. Which of those files are placed is decided from the manifest each
//! time, so editing `files`, `exclude` or `dest` needs no new resolution.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::fsutil::{io_error, write_atomic};
use crate::hash::Sha256;

const VERSION: u32 = 1;

/// The resolved models of a workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lock {
    models: Vec<LockedModel>,
}

/// A source at one revision, resolved.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedModel {
    /// The source in canonical form, as the manifest names it.
    pub source: String,
    /// The revision as the manifest names it; absent for model versions.
    pub revision: Option<String>,
    /// The commit or the version id it stands for.
    pub resolved: String,
    /// Every file the upstream lists there.
    #[serde(default)]
    pub files: Vec<LockedFile>,
}

/// A file of a resolved model.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedFile {
    /// Path in the repository, or the file name where files have ids.
    pub path: String,
    /// Id of the file, where the upstream gives files ids.
    pub file_id: Option<u64>,
    /// Size in bytes, if known.
    pub size: Option<u64>,
    /// Digest of the content. Absent until the file has been fetched once,
    /// if the upstream did not announce it.
    pub sha256: Option<Sha256>,
    /// Whether the upstream marks this as the main file.
    #[serde(default)]
    pub primary: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LockFile {
    version: u32,
    #[serde(default)]
    model: Vec<LockedModel>,
}

impl Lock {
    /// Reads the lock file at `path`. A missing file is an empty lock.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, is not valid, or was
    /// written by a newer version of this program.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(io_error(path)(error)),
        };
        let invalid = |reason: String| Error::Invalid(format!("{}: {reason}", path.display()));
        let file: LockFile = toml::from_str(&text).map_err(|e| invalid(e.to_string()))?;
        if file.version != VERSION {
            return Err(invalid(format!(
                "lock version {} is not supported (expected {VERSION})",
                file.version
            )));
        }
        Ok(Self { models: file.model })
    }

    /// Writes the lock file to `path`, in an order that does not depend on
    /// the order of changes.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, self.to_toml().as_bytes())
    }

    /// The resolution of `source` at `revision`.
    #[must_use]
    pub fn get(&self, source: &str, revision: Option<&str>) -> Option<&LockedModel> {
        self.models
            .iter()
            .find(|model| model.source == source && model.revision.as_deref() == revision)
    }

    /// The resolution of `source` at `revision`, to update it.
    pub fn get_mut(&mut self, source: &str, revision: Option<&str>) -> Option<&mut LockedModel> {
        self.models
            .iter_mut()
            .find(|model| model.source == source && model.revision.as_deref() == revision)
    }

    /// Adds a resolution, replacing the one for the same source and
    /// revision.
    pub fn insert(&mut self, model: LockedModel) {
        self.models
            .retain(|other| other.source != model.source || other.revision != model.revision);
        self.models.push(model);
    }

    /// The resolved models.
    #[must_use]
    pub fn models(&self) -> &[LockedModel] {
        &self.models
    }

    fn to_toml(&self) -> String {
        let mut models: Vec<&LockedModel> = self.models.iter().collect();
        models.sort_by(|a, b| (&a.source, &a.revision).cmp(&(&b.source, &b.revision)));

        let mut text = format!("# Written by mvault. Do not edit.\nversion = {VERSION}\n");
        for model in models {
            let _ = write!(text, "\n[[model]]\nsource = {}\n", quote(&model.source));
            if let Some(revision) = &model.revision {
                let _ = writeln!(text, "revision = {}", quote(revision));
            }
            let _ = writeln!(text, "resolved = {}\nfiles = [", quote(&model.resolved));
            let mut files: Vec<&LockedFile> = model.files.iter().collect();
            files.sort_by(|a, b| (&a.path, a.file_id).cmp(&(&b.path, b.file_id)));
            for file in files {
                let _ = write!(text, "  {{ path = {}", quote(&file.path));
                if let Some(file_id) = file.file_id {
                    let _ = write!(text, ", file_id = {file_id}");
                }
                if let Some(size) = file.size {
                    let _ = write!(text, ", size = {size}");
                }
                if let Some(sha256) = &file.sha256 {
                    let _ = write!(text, ", sha256 = \"{sha256}\"");
                }
                if file.primary {
                    text.push_str(", primary = true");
                }
                text.push_str(" },\n");
            }
            text.push_str("]\n");
        }
        text
    }
}

/// A TOML string for `value`.
fn quote(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn file(path: &str, sha256: Option<Sha256>) -> LockedFile {
        LockedFile {
            path: path.into(),
            file_id: None,
            size: Some(3),
            sha256,
            primary: false,
        }
    }

    fn sample() -> Lock {
        let mut lock = Lock::default();
        lock.insert(LockedModel {
            source: "hf:huggingface.co:example-org/example-model".into(),
            revision: Some("main".into()),
            resolved: "0123456789abcdef0123456789abcdef01234567".into(),
            files: vec![
                file("vae/model.safetensors", Some(Sha256::of(b"abc"))),
                file("config.json", None),
            ],
        });
        lock.insert(LockedModel {
            source: "civitai:example.com:12345".into(),
            revision: None,
            resolved: "67890".into(),
            files: vec![
                LockedFile {
                    file_id: Some(222),
                    size: None,
                    ..file("model \"b\".safetensors", None)
                },
                LockedFile {
                    file_id: Some(111),
                    primary: true,
                    ..file("model \"b\".safetensors", Some(Sha256::of(b"abc")))
                },
            ],
        });
        lock
    }

    #[test]
    fn a_missing_file_is_an_empty_lock() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Lock::load(&dir.path().join("mvault.lock")).unwrap(),
            Lock::default()
        );
    }

    #[test]
    fn the_file_is_sorted_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mvault.lock");
        let lock = sample();
        lock.save(&path).unwrap();

        let abc = Sha256::of(b"abc");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!(
                r#"# Written by mvault. Do not edit.
version = 1

[[model]]
source = "civitai:example.com:12345"
resolved = "67890"
files = [
  {{ path = 'model "b".safetensors', file_id = 111, size = 3, sha256 = "{abc}", primary = true }},
  {{ path = 'model "b".safetensors', file_id = 222 }},
]

[[model]]
source = "hf:huggingface.co:example-org/example-model"
revision = "main"
resolved = "0123456789abcdef0123456789abcdef01234567"
files = [
  {{ path = "config.json", size = 3 }},
  {{ path = "vae/model.safetensors", size = 3, sha256 = "{abc}" }},
]
"#
            )
        );

        let read = Lock::load(&path).unwrap();
        for model in lock.models() {
            let other = read.get(&model.source, model.revision.as_deref()).unwrap();
            assert_eq!(other.resolved, model.resolved);
            assert_eq!(other.files.len(), model.files.len());
            for file in &model.files {
                assert!(other.files.contains(file), "{file:?}");
            }
        }
    }

    #[test]
    fn models_are_found_by_source_and_revision() {
        let mut lock = sample();
        let source = "hf:huggingface.co:example-org/example-model";
        assert!(lock.get(source, Some("main")).is_some());
        assert!(lock.get(source, Some("dev")).is_none());
        assert!(lock.get(source, None).is_none());
        assert!(lock.get("civitai:example.com:12345", None).is_some());

        lock.get_mut(source, Some("main")).unwrap().files[1].sha256 = Some(Sha256::of(b"x"));
        assert_eq!(
            lock.get(source, Some("main")).unwrap().files[1].sha256,
            Some(Sha256::of(b"x"))
        );
    }

    #[test]
    fn inserting_replaces_the_same_source_and_revision() {
        let mut lock = sample();
        lock.insert(LockedModel {
            source: "civitai:example.com:12345".into(),
            revision: None,
            resolved: "70000".into(),
            files: Vec::new(),
        });
        assert_eq!(lock.models().len(), 2);
        assert_eq!(
            lock.get("civitai:example.com:12345", None)
                .unwrap()
                .resolved,
            "70000"
        );
    }

    #[test]
    fn unusable_files_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mvault.lock");
        for content in [
            "",
            "version = 2\n",
            "version = 1\nextra = true\n",
            "version = 1\n[[model]]\nsource = \"s\"\n",
            "version = 1\n[[model]]\nsource = \"s\"\nresolved = \"r\"\nfiles = [{ path = \"a\", sha256 = \"abc\" }]\n",
            "not toml",
        ] {
            fs::write(&path, content).unwrap();
            let error = Lock::load(&path).unwrap_err().to_string();
            assert!(error.contains("mvault.lock"), "{content:?}: {error}");
        }
    }
}
