//! Finding the tokens for the upstream sites.
//!
//! Tokens are never taken from the command line, where they would show up
//! in the process list and the shell history. Each one is kept in a file
//! named after the one host it is for, `~/.config/mvault/tokens/<host>`:
//! a token goes to a host only if the user put it there for that host.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::source::{DEFAULT_HF_HOST, normalize_host};

/// Looks up an environment variable. Passed in so that tests do not depend
/// on, or change, the environment of the process.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The tokens of the upstream hosts, by host.
#[derive(Default)]
pub struct Tokens(HashMap<String, String>);

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The tokens stay out of logs and panic messages.
        f.debug_set().entries(self.0.keys()).finish()
    }
}

impl Tokens {
    /// Reads the token files of `$XDG_CONFIG_HOME/mvault/tokens/` (by
    /// default `~/.config/mvault/tokens/`), one per host and named by it.
    /// Names that start with a dot are left alone.
    ///
    /// Without a file of its own, the default hub falls back to the hub's
    /// own settings: `HF_TOKEN`, else the file named by `HF_TOKEN_PATH`,
    /// else the file the hub's command-line tool writes (`$HF_HOME/token`,
    /// by default `~/.cache/huggingface/token`).
    ///
    /// # Errors
    ///
    /// Returns an error if a token file cannot be read, or if a file is not
    /// named by a host in its one spelling (it would never be used).
    pub fn load(env: Env<'_>) -> Result<Self> {
        let mut tokens = HashMap::new();
        if let Some(dir) = tokens_dir(env) {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Self::with_hf_fallback(tokens, env);
                }
                Err(source) => return Err(Error::Io { path: dir, source }),
            };
            for entry in entries {
                let entry = entry.map_err(|source| Error::Io {
                    path: dir.clone(),
                    source,
                })?;
                let path = entry.path();
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    return Err(not_a_host(&path));
                };
                if name.starts_with('.') {
                    continue;
                }
                if normalize_host(name).ok().as_deref() != Some(name) {
                    return Err(not_a_host(&path));
                }
                if let Some(token) = read_token(path)? {
                    tokens.insert(name.to_owned(), token);
                }
            }
        }
        Self::with_hf_fallback(tokens, env)
    }

    fn with_hf_fallback(mut tokens: HashMap<String, String>, env: Env<'_>) -> Result<Self> {
        if !tokens.contains_key(DEFAULT_HF_HOST)
            && let Some(token) = hf_token(env)?
        {
            tokens.insert(DEFAULT_HF_HOST.to_owned(), token);
        }
        Ok(Self(tokens))
    }

    /// The token for `host`, written in its one spelling.
    #[must_use]
    pub fn get(&self, host: &str) -> Option<&str> {
        self.0.get(host).map(String::as_str)
    }
}

fn tokens_dir(env: Env<'_>) -> Option<PathBuf> {
    let config = non_empty(env("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(non_empty(env("HOME"))?).join(".config")))?;
    Some(config.join("mvault").join("tokens"))
}

fn not_a_host(path: &Path) -> Error {
    Error::Invalid(format!(
        "{}: a token file is named by the host it is for, in lower case and without port 443",
        path.display()
    ))
}

/// The token for the default hub from the hub's own settings (see
/// [`Tokens::load`]). A token file that exists must be readable.
fn hf_token(env: Env<'_>) -> Result<Option<String>> {
    if let Some(token) = non_empty(env("HF_TOKEN")) {
        return Ok(Some(token));
    }
    let path = non_empty(env("HF_TOKEN_PATH"))
        .map(PathBuf::from)
        .or_else(|| {
            let home = non_empty(env("HF_HOME")).map(PathBuf::from).or_else(|| {
                let cache = non_empty(env("XDG_CACHE_HOME"))
                    .map(PathBuf::from)
                    .or_else(|| Some(PathBuf::from(non_empty(env("HOME"))?).join(".cache")))?;
                Some(cache.join("huggingface"))
            })?;
            Some(home.join("token"))
        });
    path.map_or(Ok(None), read_token)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The first line of a token file. A missing file is no token.
fn read_token(path: PathBuf) -> Result<Option<String>> {
    match fs::read_to_string(&path) {
        Ok(content) => Ok(non_empty(content.lines().next().map(str::to_owned))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io { path, source }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::Path;

    use super::*;

    fn env_of(vars: &[(&str, &Path)]) -> impl Fn(&str) -> Option<String> + use<> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.to_string_lossy().into_owned()))
            .collect();
        move |name| vars.get(name).cloned()
    }

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn nothing_set_means_no_token() {
        let tokens = Tokens::load(&env_of(&[])).unwrap();
        assert_eq!(tokens.get(DEFAULT_HF_HOST), None);
        assert_eq!(tokens.get("example.com"), None);
    }

    #[test]
    fn each_host_has_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let tokens_dir = home.join(".config/mvault/tokens");
        write(
            &tokens_dir.join("example.com"),
            " site-token \nsecond line\n",
        );
        write(&tokens_dir.join("127.0.0.1:8080"), "with-port");
        write(&tokens_dir.join("empty.example.com"), "\n");
        write(&tokens_dir.join(".example.com.swp"), "editor");

        let tokens = Tokens::load(&env_of(&[("HOME", &home)])).unwrap();
        assert_eq!(tokens.get("example.com"), Some("site-token"));
        assert_eq!(tokens.get("127.0.0.1:8080"), Some("with-port"));
        // An empty file is no token.
        assert_eq!(tokens.get("empty.example.com"), None);
        assert_eq!(tokens.get("other.example.net"), None);
        assert!(!format!("{tokens:?}").contains("site-token"));

        // XDG_CONFIG_HOME, when set, is where the files are.
        write(&dir.path().join("xdg/mvault/tokens/example.com"), "xdg");
        let env = env_of(&[
            ("HOME", &home),
            ("XDG_CONFIG_HOME", &dir.path().join("xdg")),
        ]);
        assert_eq!(Tokens::load(&env).unwrap().get("example.com"), Some("xdg"));
    }

    #[test]
    fn a_file_must_be_named_by_a_host_in_its_one_spelling() {
        for name in ["Example.com", "example.com:443", "bad host"] {
            let dir = tempfile::tempdir().unwrap();
            write(&dir.path().join("mvault/tokens").join(name), "token");
            let env = env_of(&[("XDG_CONFIG_HOME", dir.path())]);
            let error = Tokens::load(&env).err().unwrap().to_string();
            assert!(error.contains("named by the host"), "{name}: {error}");
        }
    }

    #[test]
    fn the_default_hub_falls_back_to_its_own_settings() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        write(&home.join(".cache/huggingface/token"), "hub-cli\n");
        let env = env_of(&[("HOME", &home), ("HF_TOKEN", Path::new("from-env"))]);
        let tokens = Tokens::load(&env).unwrap();
        assert_eq!(tokens.get(DEFAULT_HF_HOST), Some("from-env"));
        // The hub's settings are for the default hub only.
        assert_eq!(tokens.get("hub.example.com"), None);

        // A file of mvault's own wins.
        write(&home.join(".config/mvault/tokens/huggingface.co"), "own");
        let tokens = Tokens::load(&env).unwrap();
        assert_eq!(tokens.get(DEFAULT_HF_HOST), Some("own"));
    }

    #[test]
    fn hf_token_files_are_searched_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        write(&home.join(".cache/huggingface/token"), "default\n");
        write(&dir.path().join("xdg/huggingface/token"), "xdg");
        write(&dir.path().join("hf/token"), "hf-home");
        write(&dir.path().join("explicit"), "explicit\nsecond line\n");

        let mut vars = vec![("HOME", home.clone())];
        let token = |vars: &[(&str, PathBuf)]| {
            let borrowed: Vec<(&str, &Path)> =
                vars.iter().map(|(k, v)| (*k, v.as_path())).collect();
            hf_token(&env_of(&borrowed)).unwrap()
        };
        assert_eq!(token(&vars).as_deref(), Some("default"));
        vars.push(("XDG_CACHE_HOME", dir.path().join("xdg")));
        assert_eq!(token(&vars).as_deref(), Some("xdg"));
        vars.push(("HF_HOME", dir.path().join("hf")));
        assert_eq!(token(&vars).as_deref(), Some("hf-home"));
        vars.push(("HF_TOKEN_PATH", dir.path().join("explicit")));
        assert_eq!(token(&vars).as_deref(), Some("explicit"));
        // A named file that does not exist is no token; nothing else is tried.
        vars.push(("HF_TOKEN_PATH", dir.path().join("missing")));
        assert_eq!(token(&vars), None);
    }
}
