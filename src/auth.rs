//! Finding the tokens for the upstream sites.
//!
//! Tokens are never taken from the command line, where they would show up
//! in the process list and the shell history. Each one is kept in a file
//! named after the one host it is for, `~/.config/mvault/tokens/<host>`:
//! a token goes to a host only if the user put it there for that host. A
//! port is written after `_` rather than `:`, which Windows does not allow
//! in a file name (`127.0.0.1_8080` for `127.0.0.1:8080`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::source::DEFAULT_HF_HOST;

/// Looks up an environment variable. Passed in so that tests do not depend
/// on, or change, the environment of the process.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The tokens of the upstream hosts. Each is read when its host is first
/// asked for, so a host that is never reached never has its file read:
/// nothing else in the tokens directory can get in the way.
pub struct Tokens<'a> {
    env: Env<'a>,
    read: RefCell<HashMap<String, Option<String>>>,
}

impl std::fmt::Debug for Tokens<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The tokens stay out of logs and panic messages.
        f.debug_set().entries(self.read.borrow().keys()).finish()
    }
}

impl<'a> Tokens<'a> {
    /// The tokens found through `env`; nothing is read yet.
    #[must_use]
    pub fn new(env: Env<'a>) -> Self {
        Self {
            env,
            read: RefCell::default(),
        }
    }

    /// The token for `host`, written in its one spelling (lower case,
    /// without port 443). It is the first line of the file
    /// `$XDG_CONFIG_HOME/mvault/tokens/<host>` (by default
    /// `~/.config/mvault/tokens/<host>`, where `~` is `HOME`, else
    /// `USERPROFILE`), with `_` in place of the `:` before a port.
    ///
    /// Without a file of its own, the default hub falls back to the hub's
    /// own settings: `HF_TOKEN`, else the file named by `HF_TOKEN_PATH`,
    /// else the file the hub's command-line tool writes (`$HF_HOME/token`,
    /// by default `~/.cache/huggingface/token`).
    ///
    /// # Errors
    ///
    /// Returns an error if the token file of `host` exists but cannot be
    /// read, or is a symbolic link to nothing.
    pub fn get(&self, host: &str) -> Result<Option<String>> {
        if let Some(token) = self.read.borrow().get(host) {
            return Ok(token.clone());
        }
        let mut token = match tokens_dir(self.env) {
            Some(dir) => read_token(dir.join(file_name_of_host(host)))?,
            None => None,
        };
        if token.is_none() && host == DEFAULT_HF_HOST {
            token = hf_token(self.env)?;
        }
        self.read
            .borrow_mut()
            .insert(host.to_owned(), token.clone());
        Ok(token)
    }
}

/// The directory of mvault's own settings: `$XDG_CONFIG_HOME/mvault`, by
/// default `~/.config/mvault` (see [`home_dir`] for `~`).
pub(crate) fn config_dir(env: Env<'_>) -> Option<PathBuf> {
    let config = non_empty(env("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| Some(home_dir(env)?.join(".config")))?;
    Some(config.join("mvault"))
}

/// The home directory: `HOME`, else `USERPROFILE`, which Windows sets
/// where it does not set `HOME`.
fn home_dir(env: Env<'_>) -> Option<PathBuf> {
    non_empty(env("HOME"))
        .or_else(|| non_empty(env("USERPROFILE")))
        .map(PathBuf::from)
}

fn tokens_dir(env: Env<'_>) -> Option<PathBuf> {
    Some(config_dir(env)?.join("tokens"))
}

/// The name of the token file of `host`: `_` in place of the `:` before a
/// port. Host names have no `_`, so the two cannot be confused.
fn file_name_of_host(host: &str) -> String {
    host.replacen(':', "_", 1)
}

/// The token for the default hub from the hub's own settings (see
/// [`Tokens::get`]). A token file that exists must be readable.
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
                    .or_else(|| Some(home_dir(env)?.join(".cache")))?;
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

/// The first line of a token file. A missing file is no token, but a
/// symbolic link to nothing is an error: the token it was meant to lead to
/// would silently go unsent.
fn read_token(path: PathBuf) -> Result<Option<String>> {
    match fs::read_to_string(&path) {
        Ok(content) => Ok(non_empty(content.lines().next().map(str::to_owned))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if fs::symlink_metadata(&path).is_ok() {
                return Err(Error::Invalid(format!(
                    "{}: a symbolic link to a file that does not exist",
                    path.display()
                )));
            }
            Ok(None)
        }
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
        let env = env_of(&[]);
        let tokens = Tokens::new(&env);
        assert_eq!(tokens.get(DEFAULT_HF_HOST).unwrap(), None);
        assert_eq!(tokens.get("example.com").unwrap(), None);
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
        write(&tokens_dir.join("127.0.0.1_8080"), "with-port");
        write(&tokens_dir.join("empty.example.com"), "\n");

        let env = env_of(&[("HOME", &home)]);
        let tokens = Tokens::new(&env);
        assert_eq!(
            tokens.get("example.com").unwrap().as_deref(),
            Some("site-token")
        );
        assert_eq!(
            tokens.get("127.0.0.1:8080").unwrap().as_deref(),
            Some("with-port")
        );
        // An empty file is no token.
        assert_eq!(tokens.get("empty.example.com").unwrap(), None);
        assert_eq!(tokens.get("other.example.net").unwrap(), None);
        assert!(!format!("{tokens:?}").contains("site-token"));

        // XDG_CONFIG_HOME, when set, is where the files are.
        write(&dir.path().join("xdg/mvault/tokens/example.com"), "xdg");
        let env = env_of(&[
            ("HOME", &home),
            ("XDG_CONFIG_HOME", &dir.path().join("xdg")),
        ]);
        let tokens = Tokens::new(&env);
        assert_eq!(tokens.get("example.com").unwrap().as_deref(), Some("xdg"));
    }

    #[test]
    fn without_home_the_user_profile_is_home() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile");
        write(&profile.join(".config/mvault/tokens/example.com"), "site");
        write(&profile.join(".cache/huggingface/token"), "hub-cli");
        let env = env_of(&[("USERPROFILE", &profile)]);
        let tokens = Tokens::new(&env);
        assert_eq!(tokens.get("example.com").unwrap().as_deref(), Some("site"));
        assert_eq!(
            tokens.get(DEFAULT_HF_HOST).unwrap().as_deref(),
            Some("hub-cli")
        );

        // HOME, when set, comes first.
        let env = env_of(&[
            ("USERPROFILE", &profile),
            ("HOME", &dir.path().join("home")),
        ]);
        assert_eq!(Tokens::new(&env).get("example.com").unwrap(), None);
    }

    #[test]
    fn only_the_file_of_the_host_asked_for_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let tokens_dir = dir.path().join("mvault/tokens");
        write(&tokens_dir.join("example.com"), "token");
        // Files of other names, even ones no host has, are none of its business.
        write(&tokens_dir.join("example.com~"), "backup");
        write(&tokens_dir.join("#example.com#"), "autosave");
        write(&tokens_dir.join("Example.org"), "not a host's spelling");
        fs::create_dir_all(tokens_dir.join("old")).unwrap();
        // The hub's own settings are broken, but are not for this host.
        fs::create_dir_all(dir.path().join("hf-token")).unwrap();
        let env = env_of(&[
            ("XDG_CONFIG_HOME", dir.path()),
            ("HF_TOKEN_PATH", &dir.path().join("hf-token")),
        ]);
        let tokens = Tokens::new(&env);
        assert_eq!(tokens.get("example.com").unwrap().as_deref(), Some("token"));
        // Asked for, a host whose file is unreadable is an error.
        assert!(tokens.get("old").is_err());
        assert!(tokens.get(DEFAULT_HF_HOST).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_to_nothing_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let tokens_dir = dir.path().join("mvault/tokens");
        fs::create_dir_all(&tokens_dir).unwrap();
        std::os::unix::fs::symlink("a.example.com", tokens_dir.join("b.example.com")).unwrap();
        let env = env_of(&[("XDG_CONFIG_HOME", dir.path())]);
        let error = Tokens::new(&env)
            .get("b.example.com")
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("symbolic link"), "{error}");

        // A link to a file is how hosts share a token.
        write(&tokens_dir.join("a.example.com"), "shared");
        let tokens = Tokens::new(&env);
        assert_eq!(
            tokens.get("b.example.com").unwrap().as_deref(),
            Some("shared")
        );
    }

    #[test]
    fn the_default_hub_falls_back_to_its_own_settings() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        write(&home.join(".cache/huggingface/token"), "hub-cli\n");
        let env = env_of(&[("HOME", &home), ("HF_TOKEN", Path::new("from-env"))]);
        let tokens = Tokens::new(&env);
        assert_eq!(
            tokens.get(DEFAULT_HF_HOST).unwrap().as_deref(),
            Some("from-env")
        );
        // The hub's settings are for the default hub only.
        assert_eq!(tokens.get("hub.example.com").unwrap(), None);

        // A file of mvault's own wins.
        write(&home.join(".config/mvault/tokens/huggingface.co"), "own");
        let tokens = Tokens::new(&env);
        assert_eq!(tokens.get(DEFAULT_HF_HOST).unwrap().as_deref(), Some("own"));
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
