//! Finding the tokens for the upstream sites.
//!
//! Tokens are never taken from the command line, where they would show up
//! in the process list and the shell history.

use std::fs;
use std::io;
use std::path::PathBuf;

use crate::error::{Error, Result};

/// Looks up an environment variable. Passed in so that tests do not depend
/// on, or change, the environment of the process.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The token for the default hub: `HF_TOKEN`, else the file named by
/// `HF_TOKEN_PATH`, else the file the hub's own command-line tool writes
/// (`$HF_HOME/token`, by default `~/.cache/huggingface/token`).
///
/// # Errors
///
/// Returns an error if a token file exists but cannot be read.
pub fn hf_token(env: Env<'_>) -> Result<Option<String>> {
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
    path.map_or(Ok(None), |path| read_token(path, false))
}

/// The token for a Civitai compatible site: `CIVITAI_TOKEN`, else the file
/// named by `CIVITAI_TOKEN_FILE`, else `~/.config/mvault/civitai-token`.
///
/// # Errors
///
/// Returns an error if the file named by `CIVITAI_TOKEN_FILE` does not
/// exist, or if a token file cannot be read.
pub fn civitai_token(env: Env<'_>) -> Result<Option<String>> {
    if let Some(token) = non_empty(env("CIVITAI_TOKEN")) {
        return Ok(Some(token));
    }
    if let Some(path) = non_empty(env("CIVITAI_TOKEN_FILE")) {
        return read_token(PathBuf::from(path), true);
    }
    let config = non_empty(env("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(non_empty(env("HOME"))?).join(".config")));
    config.map_or(Ok(None), |config| {
        read_token(config.join("mvault").join("civitai-token"), false)
    })
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The first line of a token file. A missing file is no token, unless the
/// user named the file.
fn read_token(path: PathBuf, required: bool) -> Result<Option<String>> {
    match fs::read_to_string(&path) {
        Ok(content) => Ok(non_empty(content.lines().next().map(str::to_owned))),
        Err(error) if error.kind() == io::ErrorKind::NotFound && !required => Ok(None),
        Err(source) => Err(Error::Io { path, source }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::*;

    fn env_of(vars: &[(&str, &Path)]) -> impl Fn(&str) -> Option<String> + use<> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.to_string_lossy().into_owned()))
            .collect();
        move |name| vars.get(name).cloned()
    }

    #[test]
    fn nothing_set_means_no_token() {
        let env = env_of(&[]);
        assert_eq!(hf_token(&env).unwrap(), None);
        assert_eq!(civitai_token(&env).unwrap(), None);
    }

    #[test]
    fn the_variable_wins_over_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("token"), "from-file\n").unwrap();
        let env = env_of(&[
            ("HF_TOKEN", Path::new(" from-env ")),
            ("HF_TOKEN_PATH", &dir.path().join("token")),
            ("CIVITAI_TOKEN", Path::new("civitai-env")),
            ("CIVITAI_TOKEN_FILE", &dir.path().join("token")),
        ]);
        assert_eq!(hf_token(&env).unwrap().as_deref(), Some("from-env"));
        assert_eq!(civitai_token(&env).unwrap().as_deref(), Some("civitai-env"));
    }

    #[test]
    fn hf_token_files_are_searched_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(home.join(".cache/huggingface")).unwrap();
        fs::write(home.join(".cache/huggingface/token"), "default\n").unwrap();
        fs::create_dir_all(dir.path().join("xdg/huggingface")).unwrap();
        fs::write(dir.path().join("xdg/huggingface/token"), "xdg").unwrap();
        fs::create_dir_all(dir.path().join("hf")).unwrap();
        fs::write(dir.path().join("hf/token"), "hf-home").unwrap();
        fs::write(dir.path().join("explicit"), "explicit\nsecond line\n").unwrap();

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

    #[test]
    fn civitai_token_file_is_read_from_the_config_directory() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(home.join(".config/mvault")).unwrap();
        fs::write(home.join(".config/mvault/civitai-token"), "home-token\n").unwrap();
        fs::create_dir_all(dir.path().join("xdg/mvault")).unwrap();
        fs::write(dir.path().join("xdg/mvault/civitai-token"), "\n").unwrap();

        let env = env_of(&[("HOME", &home)]);
        assert_eq!(civitai_token(&env).unwrap().as_deref(), Some("home-token"));
        // An empty file is no token.
        let env = env_of(&[
            ("HOME", &home),
            ("XDG_CONFIG_HOME", &dir.path().join("xdg")),
        ]);
        assert_eq!(civitai_token(&env).unwrap(), None);
    }

    #[test]
    fn a_named_civitai_token_file_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_of(&[("CIVITAI_TOKEN_FILE", &dir.path().join("missing"))]);
        assert!(matches!(civitai_token(&env), Err(Error::Io { .. })));
    }
}
