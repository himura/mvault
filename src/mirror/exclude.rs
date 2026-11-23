//! Patterns for files a mirror does not fetch.

use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;

use crate::error::{Error, Result};

/// A set of exclude patterns, matched against paths relative to the root of
/// a repository.
///
/// - `/<regex>/` is a regular expression searched for anywhere in the path.
/// - A glob without `/` is matched against the name of the file and of every
///   directory above it, at any depth: `assets` excludes everything below a
///   directory of that name, `*.mp4` every such file.
/// - A glob with `/` is matched from the root of the repository against the
///   path and every directory above it. `*` does not cross a `/`; `**` does.
#[derive(Debug, Clone, Default)]
pub struct Excludes {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
enum Rule {
    Name(GlobMatcher),
    Path(GlobMatcher),
    Regex(Regex),
}

impl Excludes {
    /// Compiles the patterns.
    ///
    /// # Errors
    ///
    /// Returns an error if a pattern is empty or is not a valid glob or
    /// regular expression.
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Result<Self> {
        let rules = patterns
            .iter()
            .map(|pattern| Rule::parse(pattern.as_ref()))
            .collect::<Result<_>>()?;
        Ok(Self { rules })
    }

    /// Whether a file at `path` is excluded.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        self.rules.iter().any(|rule| rule.matches(path))
    }
}

impl Rule {
    fn parse(pattern: &str) -> Result<Self> {
        let invalid = |reason: String| {
            Error::Invalid(format!("invalid exclude pattern {pattern:?}: {reason}"))
        };
        if let Some(regex) = pattern
            .strip_prefix('/')
            .and_then(|rest| rest.strip_suffix('/'))
            .filter(|regex| !regex.is_empty())
        {
            return Regex::new(regex)
                .map(Self::Regex)
                .map_err(|e| invalid(e.to_string()));
        }
        let glob = pattern.trim_start_matches('/').trim_end_matches('/');
        if glob.is_empty() {
            return Err(invalid("the pattern is empty".into()));
        }
        let matcher = GlobBuilder::new(glob)
            .literal_separator(true)
            .build()
            .map_err(|e| invalid(e.to_string()))?
            .compile_matcher();
        Ok(if glob.contains('/') {
            Self::Path(matcher)
        } else {
            Self::Name(matcher)
        })
    }

    fn matches(&self, path: &str) -> bool {
        match self {
            Self::Regex(regex) => regex.is_match(path),
            Self::Name(glob) => path.split('/').any(|name| glob.is_match(name)),
            Self::Path(glob) => {
                glob.is_match(path)
                    || path
                        .match_indices('/')
                        .any(|(end, _)| glob.is_match(&path[..end]))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn excluded(pattern: &str, path: &str) -> bool {
        Excludes::new(&[pattern]).unwrap().matches(path)
    }

    #[test]
    fn nothing_is_excluded_without_patterns() {
        assert!(!Excludes::default().matches("model.safetensors"));
    }

    #[test]
    fn a_name_matches_at_any_depth() {
        assert!(excluded("assets", "assets/sample.mp4"));
        assert!(excluded("assets", "docs/assets/sample.mp4"));
        assert!(excluded("assets/", "assets/sample.mp4"));
        assert!(excluded("assets", "assets"));
        assert!(!excluded("assets", "assets.txt"));
        assert!(!excluded("assets", "my-assets/sample.mp4"));

        assert!(excluded("*.mp4", "sample.mp4"));
        assert!(excluded("*.mp4", "assets/deep/sample.mp4"));
        assert!(!excluded("*.mp4", "sample.mp4.txt"));
    }

    #[test]
    fn a_path_is_anchored_at_the_root() {
        assert!(excluded("assets/*.mp4", "assets/sample.mp4"));
        assert!(!excluded("assets/*.mp4", "assets/deep/sample.mp4"));
        assert!(!excluded("assets/*.mp4", "docs/assets/sample.mp4"));

        assert!(excluded("assets/**", "assets/deep/sample.mp4"));
        assert!(excluded("**/onnx/*", "a/b/onnx/model.onnx"));
        assert!(excluded("/docs/assets", "docs/assets/deep/sample.mp4"));
        assert!(!excluded("/docs/assets", "other/docs/assets/sample.mp4"));
    }

    #[test]
    fn a_regex_is_searched_in_the_whole_path() {
        assert!(excluded(r"/\.mp4$/", "assets/sample.mp4"));
        assert!(!excluded(r"/\.mp4$/", "assets/sample.mp4.txt"));
        assert!(excluded("/^assets/", "assets/sample.mp4"));
        assert!(!excluded("/^assets/", "docs/assets/sample.mp4"));
    }

    #[test]
    fn any_pattern_excludes() {
        let excludes = Excludes::new(&["assets", r"/\.onnx$/"]).unwrap();
        assert!(excludes.matches("assets/a.png"));
        assert!(excludes.matches("onnx/model.onnx"));
        assert!(!excludes.matches("model.safetensors"));
    }

    #[test]
    fn rejects_invalid_patterns() {
        for pattern in ["", "/", "//", "/(/", "a["] {
            let result = Excludes::new(&[pattern]);
            assert!(matches!(result, Err(Error::Invalid(_))), "{pattern:?}");
        }
    }
}
