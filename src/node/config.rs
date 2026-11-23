//! The settings of a node: where its mirrors are.
//!
//! Mirrors belong to the machine rather than to a workspace, so the settings
//! live in `~/.config/mvault/config.toml`. A workspace may override them in
//! `.mvault/config.toml`, key by key.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::manifest::STATE_DIR;
use crate::auth::Env;
use crate::error::{Error, Result};
use crate::fsutil::io_error;

/// The settings in effect for a workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// Mirrors to fetch from, in the order to try them: `file://` URLs or
    /// plain paths for mounted directories, `http(s)://` URLs otherwise.
    pub mirrors: Vec<String>,
    /// Whether files no mirror has may be fetched from their upstream.
    pub allow_official: bool,
    /// Directory of the blobs, if not `.mvault/blobs` in the workspace.
    pub blobs: Option<PathBuf>,
    /// The host assumed for `civitai:` sources that name none, and the only
    /// one the site token is sent to.
    pub civitai_host: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    mirrors: Option<Vec<String>>,
    allow_official: Option<bool>,
    blobs: Option<PathBuf>,
    #[serde(default)]
    civitai: RawCivitai,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCivitai {
    default_host: Option<String>,
}

impl Config {
    /// Reads the settings of the node from `global`, if given, and those of
    /// `workspace` over them.
    ///
    /// # Errors
    ///
    /// Returns an error if a file exists but cannot be read or has content
    /// that is not understood.
    pub fn load(global: Option<&Path>, workspace: &Path) -> Result<Self> {
        let mut config = Self::default();
        if let Some(global) = global {
            config.apply(read(global)?, workspace);
        }
        let local = workspace.join(STATE_DIR).join("config.toml");
        config.apply(read(&local)?, workspace);
        Ok(config)
    }

    fn apply(&mut self, raw: RawConfig, workspace: &Path) {
        if let Some(mirrors) = raw.mirrors {
            self.mirrors = mirrors;
        }
        if let Some(allow_official) = raw.allow_official {
            self.allow_official = allow_official;
        }
        if let Some(blobs) = raw.blobs {
            // A relative path is meant from the workspace.
            self.blobs = Some(workspace.join(blobs));
        }
        if let Some(host) = raw.civitai.default_host {
            self.civitai_host = Some(host);
        }
    }
}

/// The path of the settings of the node: `$XDG_CONFIG_HOME/mvault/config.toml`,
/// by default `~/.config/mvault/config.toml`.
#[must_use]
pub fn global_path(env: Env<'_>) -> Option<PathBuf> {
    let set = |name: &str| {
        env(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let config = set("XDG_CONFIG_HOME").or_else(|| Some(set("HOME")?.join(".config")))?;
    Some(config.join("mvault").join("config.toml"))
}

fn read(path: &Path) -> Result<RawConfig> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(RawConfig::default()),
        Err(error) => return Err(io_error(path)(error)),
    };
    toml::from_str(&text).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn nothing_configured_gives_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("none.toml");
        assert_eq!(Config::load(None, dir.path()).unwrap(), Config::default());
        assert_eq!(
            Config::load(Some(&missing), dir.path()).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn the_workspace_overrides_the_node_key_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let global = dir.path().join("global.toml");
        write(
            &global,
            "mirrors = [\"file:///mnt/vault/public\", \"https://mirror.example.internal/\"]\n\
             allow_official = true\n[civitai]\ndefault_host = \"example.com\"\n",
        );

        let node = Config::load(Some(&global), &workspace).unwrap();
        assert_eq!(
            node.mirrors,
            [
                "file:///mnt/vault/public",
                "https://mirror.example.internal/"
            ]
        );
        assert!(node.allow_official);
        assert_eq!(node.blobs, None);
        assert_eq!(node.civitai_host.as_deref(), Some("example.com"));

        write(
            &workspace.join(".mvault/config.toml"),
            "mirrors = [\"/mnt/other\"]\nblobs = \"shared/blobs\"\n",
        );
        let local = Config::load(Some(&global), &workspace).unwrap();
        assert_eq!(local.mirrors, ["/mnt/other"]);
        assert!(local.allow_official);
        assert_eq!(local.blobs, Some(workspace.join("shared/blobs")));
        assert_eq!(local.civitai_host.as_deref(), Some("example.com"));
    }

    #[test]
    fn unknown_keys_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".mvault/config.toml"), "mirror = \"x\"\n");
        let error = Config::load(None, dir.path()).unwrap_err().to_string();
        assert!(error.contains("config.toml"), "{error}");
    }

    #[test]
    fn the_node_settings_are_found_through_the_environment() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        assert_eq!(global_path(&env(&[])), None);
        assert_eq!(
            global_path(&env(&[("HOME", "/home/user")])),
            Some(PathBuf::from("/home/user/.config/mvault/config.toml"))
        );
        assert_eq!(
            global_path(&env(&[("HOME", "/home/user"), ("XDG_CONFIG_HOME", "/xdg")])),
            Some(PathBuf::from("/xdg/mvault/config.toml"))
        );
    }
}
