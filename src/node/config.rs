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
use crate::auth::{Env, config_dir};
use crate::error::{Error, Result};
use crate::fsutil::io_error;
use crate::source::normalize_host;

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
    /// The host assumed for `civitai:` sources that name none.
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
            config.apply(read(global)?, workspace, false, global)?;
        }
        let local = workspace.join(STATE_DIR).join("config.toml");
        config.apply(read(&local)?, workspace, true, &local)?;
        Ok(config)
    }

    fn apply(&mut self, raw: RawConfig, workspace: &Path, local: bool, file: &Path) -> Result<()> {
        let invalid = |reason: String| Error::Invalid(format!("{}: {reason}", file.display()));
        if let Some(mirrors) = raw.mirrors {
            let mut resolved = Vec::new();
            for mirror in mirrors {
                // A relative path means the same from wherever mvault runs:
                // from the workspace, like `blobs`. The settings of the node
                // serve every workspace, so there it has no meaning.
                let is_path = !mirror.contains("://");
                if is_path && !Path::new(&mirror).has_root() && !mirror.is_empty() {
                    if !local {
                        return Err(invalid(format!(
                            "mirror {mirror:?} is a relative path; give an absolute one"
                        )));
                    }
                    resolved.push(workspace.join(&mirror).to_string_lossy().into_owned());
                } else {
                    resolved.push(mirror);
                }
            }
            self.mirrors = resolved;
        }
        if let Some(allow_official) = raw.allow_official {
            self.allow_official = allow_official;
        }
        if let Some(blobs) = raw.blobs {
            // A relative path is meant from the workspace.
            self.blobs = Some(workspace.join(blobs));
        }
        if let Some(host) = raw.civitai.default_host {
            // Taken into sources, which spell a host one way.
            let host =
                normalize_host(&host).map_err(|e| invalid(format!("civitai.default_host: {e}")))?;
            self.civitai_host = Some(host);
        }
        Ok(())
    }
}

/// The path of the settings of the node: `$XDG_CONFIG_HOME/mvault/config.toml`,
/// by default `~/.config/mvault/config.toml`, where `~` is `HOME`, else
/// `USERPROFILE`.
#[must_use]
pub fn global_path(env: Env<'_>) -> Option<PathBuf> {
    Some(config_dir(env)?.join("config.toml"))
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
    fn a_relative_mirror_is_meant_from_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        write(
            &workspace.join(".mvault/config.toml"),
            "mirrors = [\"../vault/public\", \"https://mirror.example.internal\"]\n",
        );
        let config = Config::load(None, &workspace).unwrap();
        assert_eq!(
            config.mirrors,
            [
                workspace
                    .join("../vault/public")
                    .to_string_lossy()
                    .into_owned(),
                "https://mirror.example.internal".to_owned()
            ]
        );

        let global = dir.path().join("global.toml");
        write(&global, "mirrors = [\"vault/public\"]\n");
        let error = Config::load(Some(&global), &workspace)
            .unwrap_err()
            .to_string();
        assert!(error.contains("relative path"), "{error}");
    }

    #[test]
    fn the_default_host_is_written_one_way() {
        let dir = tempfile::tempdir().unwrap();
        for (written, expected) in [
            ("Example.com", "example.com"),
            ("example.com:443", "example.com"),
            ("example.com:8443", "example.com:8443"),
        ] {
            write(
                &dir.path().join(".mvault/config.toml"),
                &format!("[civitai]\ndefault_host = \"{written}\"\n"),
            );
            let config = Config::load(None, dir.path()).unwrap();
            assert_eq!(config.civitai_host.as_deref(), Some(expected), "{written}");
        }
        write(
            &dir.path().join(".mvault/config.toml"),
            "[civitai]\ndefault_host = \"bad host\"\n",
        );
        assert!(Config::load(None, dir.path()).is_err());
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
            global_path(&env(&[("USERPROFILE", "/profile")])),
            Some(PathBuf::from("/profile/.config/mvault/config.toml"))
        );
        assert_eq!(
            global_path(&env(&[("HOME", "/home/user"), ("XDG_CONFIG_HOME", "/xdg")])),
            Some(PathBuf::from("/xdg/mvault/config.toml"))
        );
    }
}
