//! The workspace: the directory with the manifest, where models are placed.
//!
//! ```text
//! <workspace>/mvault.toml    manifest       (version controlled)
//! <workspace>/mvault.lock    lock file      (version controlled)
//! <workspace>/.mvault/       blobs/, tmp/, placed.json, lock, config.toml
//!                            (this node only)
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use super::config::Config;
use super::manifest::{LOCK_NAME, MANIFEST_NAME, STATE_DIR};
use super::placed::PLACED_NAME;
use super::remote::Remote;
use crate::error::{Error, Result};
use crate::fsutil::{io_error, wait_for_lock, write_atomic};
use crate::source::Defaults;
use crate::store::BlobStore;

/// Tells the log where the workspace is and which settings are in effect.
fn log_settings(root: &Path, config: &Config, blobs: &Path) {
    log::debug!("workspace {}", root.display());
    log::debug!("blobs in {}", blobs.display());
    if config.mirrors.is_empty() {
        log::debug!("no mirrors");
    }
    for mirror in &config.mirrors {
        let shown = if mirror.contains("://") {
            crate::download::redact(mirror)
        } else {
            mirror.clone()
        };
        log::debug!("mirror {shown}");
    }
    log::debug!(
        "fetching from the upstream is {}allowed (allow_official)",
        if config.allow_official { "" } else { "not " }
    );
    if let Some(host) = &config.civitai_host {
        log::debug!("default Civitai host {host}");
    }
}

const MANIFEST_TEMPLATE: &str = "\
# The models of this workspace. `mvault add <URL> --dest <dir>/` appends an
# entry; `mvault sync` places the files. dest is relative to this directory.
";

/// An opened workspace.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// The directory of the workspace.
    pub root: PathBuf,
    /// The settings in effect.
    pub config: Config,
    /// The blobs of this node.
    pub store: BlobStore,
}

impl Workspace {
    /// The workspace `start` is in: the nearest directory, upwards from
    /// `start`, that has a manifest.
    ///
    /// # Errors
    ///
    /// Returns an error if there is none.
    pub fn find(start: &Path) -> Result<PathBuf> {
        start
            .ancestors()
            .find(|dir| dir.join(MANIFEST_NAME).is_file())
            .map(Path::to_owned)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "no {MANIFEST_NAME} in {} or above it (mvault init creates one)",
                    start.display()
                ))
            })
    }

    /// Opens the workspace at `root` with the settings of the node at
    /// `global_config`, if given.
    ///
    /// # Errors
    ///
    /// Returns an error if the settings cannot be read.
    pub fn open(root: PathBuf, global_config: Option<&Path>) -> Result<Self> {
        let config = Config::load(global_config, &root)?;
        let blobs = config
            .blobs
            .clone()
            .unwrap_or_else(|| root.join(STATE_DIR).join("blobs"));
        // Unfinished files go next to the blobs, on the same filesystem.
        let tmp = blobs.parent().unwrap_or(&root).join("tmp");
        log_settings(&root, &config, &blobs);
        Ok(Self {
            store: BlobStore::new(blobs, tmp),
            root,
            config,
        })
    }

    /// The path of the lock file.
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.root.join(LOCK_NAME)
    }

    /// The path of the record of placed files.
    #[must_use]
    pub fn placed_path(&self) -> PathBuf {
        self.root.join(STATE_DIR).join(PLACED_NAME)
    }

    /// Waits for the lock that lets one command at a time change the
    /// workspace, and holds it until the returned file is dropped.
    ///
    /// Without `create`, for a command that changes nothing, the lock file
    /// is not created: if there is none, no command changing the workspace
    /// has ever run, and `None` is returned.
    ///
    /// # Errors
    ///
    /// Returns an error if the lock file cannot be created or locked.
    pub fn lock(&self, create: bool) -> Result<Option<fs::File>> {
        let state = self.root.join(STATE_DIR);
        let path = state.join(WORKSPACE_LOCK);
        if create {
            fs::create_dir_all(&state).map_err(io_error(&state))?;
        }
        let file = fs::File::options()
            .create(create)
            .truncate(false)
            .write(true)
            .open(&path);
        let file = match file {
            Ok(file) => file,
            Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(io_error(&path)(error)),
        };
        wait_for_lock(&file, &path, create)?;
        Ok(Some(file))
    }

    /// Checks that no destination is inside the blobs or the directory of
    /// unfinished files: placing or pruning there would change the store.
    ///
    /// # Errors
    ///
    /// Returns an error naming the first such destination.
    pub fn check_dests<'a>(&self, dests: impl IntoIterator<Item = &'a str>) -> Result<()> {
        let blobs = normalize(
            &self
                .store
                .blob_path(&crate::hash::Sha256::from_bytes([0; 32])),
        );
        // `<blobs>/sha256/00/<digest>`, three levels below the blobs.
        let blobs = blobs.ancestors().nth(3).unwrap_or(&blobs).to_owned();
        let tmp = normalize(self.store.tmp_dir());
        for dest in dests {
            let path = normalize(&self.root.join(dest));
            if path.starts_with(&blobs) || path.starts_with(&tmp) {
                return Err(Error::Invalid(format!(
                    "{dest} is inside the blobs of this node ({} and {}); \
                     move the blobs or the destination",
                    blobs.display(),
                    tmp.display()
                )));
            }
        }
        Ok(())
    }

    /// The hosts to assume for sources of the manifest that name none.
    #[must_use]
    pub fn defaults(&self) -> Defaults {
        Defaults {
            civitai_host: self.config.civitai_host.clone(),
        }
    }

    /// The mirrors of the settings.
    ///
    /// # Errors
    ///
    /// Returns an error if a mirror is not a path or a URL of a known kind.
    pub fn remotes(&self) -> Result<Vec<Remote>> {
        self.config
            .mirrors
            .iter()
            .map(|location| Remote::parse(location))
            .collect()
    }
}

/// Name of the lock file of a workspace, in its state directory.
const WORKSPACE_LOCK: &str = "lock";

/// `path` without `.` and with `..` taken away where it can be, without
/// asking the filesystem.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normal.pop() {
                    normal.push("..");
                }
            }
            other => normal.push(other),
        }
    }
    normal
}

/// Makes `dir` a workspace: creates the manifest and the directory for this
/// node's state, unless they exist. With `mirrors` or `allow_official`, also
/// writes the workspace's own settings.
///
/// Returns the paths that were created.
///
/// # Errors
///
/// Returns an error if something cannot be written, or if settings are
/// given but the workspace already has its own.
pub fn init(dir: &Path, mirrors: &[String], allow_official: bool) -> Result<Vec<PathBuf>> {
    let mut created = Vec::new();
    let state = dir.join(STATE_DIR);
    let config = state.join("config.toml");
    let wants_config = !mirrors.is_empty() || allow_official;
    if wants_config && config.exists() {
        return Err(Error::Invalid(format!(
            "{} exists; edit it instead",
            config.display()
        )));
    }
    for mirror in mirrors {
        Remote::parse(mirror)?;
    }

    fs::create_dir_all(&state).map_err(io_error(&state))?;
    let manifest = dir.join(MANIFEST_NAME);
    // Nothing in the state directory is for version control.
    let ignore = state.join(".gitignore");
    for (path, content) in [(manifest, MANIFEST_TEMPLATE), (ignore, "*\n")] {
        if !path.exists() {
            write_atomic(&path, content.as_bytes())?;
            created.push(path);
        }
    }
    if wants_config {
        let mut lines = Vec::new();
        if !mirrors.is_empty() {
            let list =
                toml::Value::Array(mirrors.iter().cloned().map(toml::Value::String).collect());
            lines.push(format!("mirrors = {list}\n"));
        }
        if allow_official {
            lines.push("allow_official = true\n".to_owned());
        }
        write_atomic(&config, lines.concat().as_bytes())?;
        created.push(config);
    }
    Ok(created)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_a_workspace_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("mlenv");

        let created = init(&root, &[], false).unwrap();
        assert_eq!(
            created,
            [root.join("mvault.toml"), root.join(".mvault/.gitignore")]
        );
        assert_eq!(
            fs::read_to_string(root.join(".mvault/.gitignore")).unwrap(),
            "*\n"
        );

        fs::write(root.join("mvault.toml"), "# mine\n").unwrap();
        assert_eq!(init(&root, &[], false).unwrap(), Vec::<PathBuf>::new());
        assert_eq!(
            fs::read_to_string(root.join("mvault.toml")).unwrap(),
            "# mine\n"
        );
    }

    #[test]
    fn init_writes_settings_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let mirrors = [
            "/mnt/vault/public".to_owned(),
            "https://mirror.example.internal".to_owned(),
        ];

        init(dir.path(), &mirrors, true).unwrap();

        let workspace = Workspace::open(dir.path().to_owned(), None).unwrap();
        assert_eq!(workspace.config.mirrors, mirrors);
        assert!(workspace.config.allow_official);
        assert_eq!(
            workspace.remotes().unwrap(),
            [
                Remote::Dir("/mnt/vault/public".into()),
                Remote::Http("https://mirror.example.internal".into())
            ]
        );
        // Existing settings are not replaced.
        assert!(init(dir.path(), &mirrors, false).is_err());
        assert!(init(dir.path(), &[], false).is_ok());
    }

    #[test]
    fn init_rejects_a_mirror_of_an_unknown_kind() {
        let dir = tempfile::tempdir().unwrap();
        assert!(init(dir.path(), &["ftp://example.com".to_owned()], false).is_err());
        assert!(!dir.path().join("mvault.toml").exists());
    }

    #[test]
    fn the_workspace_is_found_from_below() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path(), &[], false).unwrap();
        let below = dir.path().join("models/vae");
        fs::create_dir_all(&below).unwrap();

        assert_eq!(Workspace::find(&below).unwrap(), dir.path());
        assert_eq!(Workspace::find(dir.path()).unwrap(), dir.path());
        let outside = tempfile::tempdir().unwrap();
        assert!(Workspace::find(outside.path()).is_err());
    }

    #[test]
    fn blobs_and_unfinished_files_are_side_by_side() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path().to_owned(), None).unwrap();
        let digest = crate::hash::Sha256::of(b"abc");
        assert!(
            workspace
                .store
                .blob_path(&digest)
                .starts_with(dir.path().join(".mvault/blobs"))
        );
        assert!(
            workspace
                .store
                .partial_path("x")
                .unwrap()
                .starts_with(dir.path().join(".mvault/tmp"))
        );

        fs::write(
            dir.path().join(".mvault/config.toml"),
            "blobs = \"shared/blobs\"\n",
        )
        .unwrap();
        let shared = Workspace::open(dir.path().to_owned(), None).unwrap();
        assert!(
            shared
                .store
                .blob_path(&digest)
                .starts_with(dir.path().join("shared/blobs"))
        );
        assert!(
            shared
                .store
                .partial_path("x")
                .unwrap()
                .starts_with(dir.path().join("shared/tmp"))
        );
    }
}
