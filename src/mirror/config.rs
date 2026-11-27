//! The settings of a mirror, `<root>/private/config.toml`.

use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::fsutil::io_error;
use crate::source::normalize_host;

/// The settings of a mirror. Every one of them is optional, and so is the
/// file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Settings for Civitai compatible sites.
    #[serde(default)]
    pub civitai: CivitaiConfig,
}

/// Settings for Civitai compatible sites.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CivitaiConfig {
    /// The host assumed when a source does not name one.
    pub default_host: Option<String>,
}

impl Config {
    /// Reads the settings of the mirror at `root`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or has
    /// content that is not understood.
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("private").join("config.toml");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(io_error(&path)(error)),
        };
        let invalid = |reason: String| Error::Invalid(format!("{}: {reason}", path.display()));
        let mut config: Self = toml::from_str(&text).map_err(|e| invalid(e.to_string()))?;
        // Taken into sources, which spell a host one way.
        if let Some(host) = &config.civitai.default_host {
            config.civitai.default_host =
                Some(normalize_host(host).map_err(|e| invalid(format!("default_host: {e}")))?);
        }
        Ok(config)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn load(content: Option<&str>) -> Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        if let Some(content) = content {
            fs::create_dir_all(dir.path().join("private")).unwrap();
            fs::write(dir.path().join("private/config.toml"), content).unwrap();
        }
        Config::load(dir.path())
    }

    #[test]
    fn a_missing_or_empty_file_gives_the_defaults() {
        assert_eq!(load(None).unwrap(), Config::default());
        assert_eq!(load(Some("")).unwrap(), Config::default());
        assert_eq!(load(Some("[civitai]\n")).unwrap(), Config::default());
    }

    #[test]
    fn the_default_host_is_read() {
        let config = load(Some("[civitai]\ndefault_host = \"example.com\"\n")).unwrap();
        assert_eq!(config.civitai.default_host.as_deref(), Some("example.com"));
    }

    #[test]
    fn the_default_host_is_written_one_way() {
        for written in ["Example.com", "example.com:443", "EXAMPLE.COM"] {
            let config = load(Some(&format!("[civitai]\ndefault_host = \"{written}\"\n"))).unwrap();
            assert_eq!(
                config.civitai.default_host.as_deref(),
                Some("example.com"),
                "{written}"
            );
        }
        assert!(load(Some("[civitai]\ndefault_host = \"bad host\"\n")).is_err());
    }

    #[test]
    fn unknown_keys_and_malformed_files_are_errors() {
        for content in ["unknown = 1\n", "[civitai]\ntoken = \"x\"\n", "[civitai\n"] {
            let error = load(Some(content)).unwrap_err();
            assert!(error.to_string().contains("config.toml"), "{error}");
        }
    }
}
