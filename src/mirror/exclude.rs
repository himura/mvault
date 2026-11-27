//! Patterns for files a mirror does not fetch, or a whole-repository entry
//! of a manifest does not place.

use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;

use crate::error::{Error, Result};

/// A set of exclude patterns, matched against paths relative to the root of
/// a repository. Whether a pattern is a glob or a regular expression is
/// told by where it is given (`--exclude` / `--exclude-regexp`, `exclude` /
/// `exclude_regexp`), never by how it is written.
///
/// Globs mean what they mean in `.gitignore`:
///
/// - A glob without `/` is matched against the name of the file and of every
///   directory above it, at any depth: `assets` excludes everything below a
///   directory of that name, `*.mp4` every such file.
/// - A glob with a `/` at the start or in the middle is matched from the
///   root of the repository against the path and every directory above it:
///   `/assets` is the `assets` at the root only, and `docs/assets` is not
///   `x/docs/assets`.
/// - A glob ending in `/` only matches directories.
/// - `*` does not cross a `/`; `**` does.
///
/// A regular expression is searched for anywhere in the path.
#[derive(Debug, Clone, Default)]
pub struct Excludes {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
enum Rule {
    Glob {
        matcher: GlobMatcher,
        anchored: bool,
        dir_only: bool,
    },
    Regex(Regex),
}

impl Excludes {
    /// Compiles the globs and the regular expressions.
    ///
    /// # Errors
    ///
    /// Returns an error if a pattern is empty or is not a valid glob or
    /// regular expression.
    pub fn new<G: AsRef<str>, R: AsRef<str>>(globs: &[G], regexps: &[R]) -> Result<Self> {
        let mut rules = globs
            .iter()
            .map(|glob| Rule::glob(glob.as_ref()))
            .collect::<Result<Vec<_>>>()?;
        for regexp in regexps {
            rules.push(Rule::regex(regexp.as_ref())?);
        }
        Ok(Self { rules })
    }

    /// Whether a file at `path` is excluded.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        self.rules.iter().any(|rule| rule.matches(path))
    }
}

impl Rule {
    fn glob(pattern: &str) -> Result<Self> {
        let invalid =
            |reason: String| Error::Invalid(format!("invalid exclude glob {pattern:?}: {reason}"));
        let dir_only = pattern.ends_with('/');
        let body = pattern.trim_end_matches('/');
        let anchored = body.contains('/');
        let glob = body.trim_start_matches('/');
        if glob.is_empty() {
            return Err(invalid("the pattern is empty".into()));
        }
        let matcher = GlobBuilder::new(glob)
            .literal_separator(true)
            .build()
            .map_err(|e| invalid(e.to_string()))?
            .compile_matcher();
        Ok(Self::Glob {
            matcher,
            anchored,
            dir_only,
        })
    }

    fn regex(pattern: &str) -> Result<Self> {
        let invalid = |reason: String| {
            Error::Invalid(format!(
                "invalid exclude regular expression {pattern:?}: {reason}"
            ))
        };
        if pattern.is_empty() {
            return Err(invalid("the pattern is empty".into()));
        }
        Regex::new(pattern)
            .map(Self::Regex)
            .map_err(|e| invalid(e.to_string()))
    }

    fn matches(&self, path: &str) -> bool {
        match self {
            Self::Regex(regex) => regex.is_match(path),
            Self::Glob {
                matcher,
                anchored,
                dir_only,
            } => {
                // The directories above the file, and the file itself
                // unless only directories are meant.
                let mut prefixes: Vec<&str> = path
                    .match_indices('/')
                    .map(|(end, _)| &path[..end])
                    .collect();
                if !dir_only {
                    prefixes.push(path);
                }
                prefixes.into_iter().any(|prefix| {
                    if *anchored {
                        matcher.is_match(prefix)
                    } else {
                        let name = prefix.rsplit_once('/').map_or(prefix, |(_, name)| name);
                        matcher.is_match(name)
                    }
                })
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const NONE: &[&str] = &[];

    fn excluded(pattern: &str, path: &str) -> bool {
        Excludes::new(&[pattern], NONE).unwrap().matches(path)
    }

    fn excluded_by_regexp(pattern: &str, path: &str) -> bool {
        Excludes::new(NONE, &[pattern]).unwrap().matches(path)
    }

    #[test]
    fn nothing_is_excluded_without_patterns() {
        assert!(!Excludes::default().matches("model.safetensors"));
    }

    #[test]
    fn a_name_matches_at_any_depth() {
        assert!(excluded("assets", "assets/sample.mp4"));
        assert!(excluded("assets", "docs/assets/sample.mp4"));
        assert!(excluded("assets", "assets"));
        assert!(!excluded("assets", "assets.txt"));
        assert!(!excluded("assets", "my-assets/sample.mp4"));

        assert!(excluded("*.mp4", "sample.mp4"));
        assert!(excluded("*.mp4", "assets/deep/sample.mp4"));
        assert!(!excluded("*.mp4", "sample.mp4.txt"));
    }

    #[test]
    fn a_trailing_slash_means_a_directory() {
        assert!(excluded("assets/", "assets/sample.mp4"));
        assert!(excluded("assets/", "docs/assets/sample.mp4"));
        assert!(!excluded("assets/", "assets"));
        assert!(!excluded("assets/", "docs/assets"));
    }

    #[test]
    fn a_slash_at_the_start_or_in_the_middle_anchors_at_the_root() {
        assert!(excluded("/assets", "assets/sample.mp4"));
        assert!(!excluded("/assets", "docs/assets/sample.mp4"));
        assert!(excluded("/assets/", "assets/sample.mp4"));

        assert!(excluded("assets/*.mp4", "assets/sample.mp4"));
        assert!(!excluded("assets/*.mp4", "assets/deep/sample.mp4"));
        assert!(!excluded("assets/*.mp4", "docs/assets/sample.mp4"));

        assert!(excluded("assets/**", "assets/deep/sample.mp4"));
        assert!(excluded("**/onnx/*", "a/b/onnx/model.onnx"));
        assert!(excluded("/docs/assets", "docs/assets/deep/sample.mp4"));
        assert!(!excluded("/docs/assets", "other/docs/assets/sample.mp4"));
        assert!(!excluded("docs/assets", "other/docs/assets/sample.mp4"));
    }

    #[test]
    fn a_glob_is_never_taken_for_a_regular_expression() {
        assert!(!excluded("/\\.mp4$/", "assets/sample.mp4"));
    }

    #[test]
    fn a_regular_expression_is_searched_in_the_whole_path() {
        assert!(excluded_by_regexp(r"\.mp4$", "assets/sample.mp4"));
        assert!(!excluded_by_regexp(r"\.mp4$", "assets/sample.mp4.txt"));
        assert!(excluded_by_regexp("^assets", "assets/sample.mp4"));
        assert!(!excluded_by_regexp("^assets", "docs/assets/sample.mp4"));
        // A slash is part of the expression, not a delimiter.
        assert!(excluded_by_regexp("/onnx/", "a/onnx/model.onnx"));
    }

    #[test]
    fn any_pattern_excludes() {
        let excludes = Excludes::new(&["assets"], &[r"\.onnx$"]).unwrap();
        assert!(excludes.matches("assets/a.png"));
        assert!(excludes.matches("onnx/model.onnx"));
        assert!(!excludes.matches("model.safetensors"));
    }

    #[test]
    fn rejects_invalid_patterns() {
        for pattern in ["", "/", "//", "a["] {
            let result = Excludes::new(&[pattern], NONE);
            assert!(matches!(result, Err(Error::Invalid(_))), "{pattern:?}");
        }
        for pattern in ["", "("] {
            let result = Excludes::new(NONE, &[pattern]);
            assert!(matches!(result, Err(Error::Invalid(_))), "{pattern:?}");
        }
    }
}
