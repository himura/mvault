//! Where a model comes from, and the notation for it.
//!
//! The canonical forms are `hf:<host>:<org>/<repo>` and
//! `civitai:<host>:<modelId>[@<versionId>]`. Page URLs of either kind of site
//! are accepted as input and reduced to the canonical form.

use std::fmt;

use percent_encoding::percent_decode_str;

/// The host assumed for a `hf:` source that does not name one.
pub const DEFAULT_HF_HOST: &str = "huggingface.co";

const HTTPS_PORT: u16 = 443;
const HTTP_PORT: u16 = 80;

/// The origin of a model: a repository or a model version on an upstream host.
///
/// Every host is its own namespace; two hosts never share an identity, even if
/// they happen to serve the same ids.
///
/// A host has exactly one spelling, so that it can serve as a key and as a
/// directory name: lowercase, no trailing dot, and the port in decimal without
/// leading zeros. The port is left out if it is 443, and a URL may also leave
/// out (or spell out) the default port of its own scheme.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// A model repository on a Hugging Face compatible hub.
    HuggingFace {
        /// Host of the hub, lowercase, with the port if it is not the default
        /// one (see [`Source`] on what counts as the default).
        host: String,
        /// Repository id: `<org>/<repo>`, or `<repo>` alone.
        repo: String,
    },
    /// A model on a Civitai compatible site.
    Civitai {
        /// Host of the site, lowercase, with the port if it is not the default
        /// one (see [`Source`] on what counts as the default).
        host: String,
        /// Id of the model.
        model_id: u64,
        /// Id of the model version; `None` stands for the latest one.
        version_id: Option<u64>,
    },
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HuggingFace { host, repo } => write!(f, "hf:{host}:{repo}"),
            Self::Civitai {
                host,
                model_id,
                version_id: Some(version_id),
            } => write!(f, "civitai:{host}:{model_id}@{version_id}"),
            Self::Civitai {
                host,
                model_id,
                version_id: None,
            } => write!(f, "civitai:{host}:{model_id}"),
        }
    }
}

/// A source together with whatever else the input said about it.
///
/// A canonical form only names the [`Source`]; a URL may also name a revision,
/// a file in the repository, or a file of a model version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The source.
    pub source: Source,
    /// Branch, tag or commit of a Hugging Face repository.
    pub revision: Option<String>,
    /// Path of a file in a Hugging Face repository.
    pub path: Option<String>,
    /// Id of a file of a Civitai model version.
    pub file_id: Option<u64>,
}

/// Hosts to assume when the input does not name one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Defaults {
    /// Host for `civitai:<modelId>…` without a host. There is no built-in one.
    pub civitai_host: Option<String>,
}

/// The error returned when a string does not describe a source.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid source {input:?}: {reason}")]
pub struct ParseError {
    input: String,
    reason: String,
}

type Reason = String;

impl Reference {
    /// Parses a canonical form or a page URL.
    ///
    /// # Errors
    ///
    /// Returns an error if `input` is neither, or if any part of it could not
    /// be used safely as a path component later on.
    pub fn parse(input: &str, defaults: &Defaults) -> Result<Self, ParseError> {
        let parsed = if let Some(rest) = input.strip_prefix("hf:") {
            parse_hf(rest).map(Self::from)
        } else if let Some(rest) = input.strip_prefix("civitai:") {
            parse_civitai(rest, defaults).map(Self::from)
        } else if let Some((default_port, rest)) = strip_scheme(input) {
            parse_url(rest, default_port)
        } else {
            Err("expected hf:<org>/<repo>, civitai:<host>:<modelId>@<versionId> or a URL".into())
        };
        parsed.map_err(|reason| ParseError {
            input: input.to_owned(),
            reason,
        })
    }
}

impl From<Source> for Reference {
    fn from(source: Source) -> Self {
        Self {
            source,
            revision: None,
            path: None,
            file_id: None,
        }
    }
}

fn parse_hf(rest: &str) -> Result<Source, Reason> {
    let (host, repo) = match rest.rsplit_once(':') {
        Some((host, repo)) => (normalize_host(host, HTTPS_PORT)?, repo),
        None => (DEFAULT_HF_HOST.to_owned(), rest),
    };
    let segments: Vec<&str> = repo.split('/').collect();
    if segments.len() > 2 {
        return Err(format!("repository {repo:?} has more than two parts"));
    }
    for segment in &segments {
        check_repo_segment(segment)?;
    }
    Ok(Source::HuggingFace {
        host,
        repo: repo.to_owned(),
    })
}

fn parse_civitai(rest: &str, defaults: &Defaults) -> Result<Source, Reason> {
    let (host, ids) = match rest.rsplit_once(':') {
        Some((host, ids)) => (normalize_host(host, HTTPS_PORT)?, ids),
        None => match &defaults.civitai_host {
            Some(host) => (normalize_host(host, HTTPS_PORT)?, rest),
            None => return Err("no host given and no default civitai host is configured".into()),
        },
    };
    let (model_id, version_id) = match ids.split_once('@') {
        Some((model, version)) => (
            parse_id("model id", model)?,
            Some(parse_id("version id", version)?),
        ),
        None => (parse_id("model id", ids)?, None),
    };
    Ok(Source::Civitai {
        host,
        model_id,
        version_id,
    })
}

/// The default port of the scheme and the part after `://`, if `input` is an
/// http(s) URL.
fn strip_scheme(input: &str) -> Option<(u16, &str)> {
    let (scheme, rest) = input.split_once("://")?;
    if scheme.eq_ignore_ascii_case("https") {
        Some((HTTPS_PORT, rest))
    } else if scheme.eq_ignore_ascii_case("http") {
        Some((HTTP_PORT, rest))
    } else {
        None
    }
}

fn parse_url(rest: &str, default_port: u16) -> Result<Reference, Reason> {
    let rest = rest.split_once('#').map_or(rest, |(before, _)| before);
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = normalize_host(authority, default_port)?;
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(decode_segment)
        .collect::<Result<Vec<_>, _>>()?;

    // A site of the civitai kind is recognized by the shape of its model pages,
    // since no host of that kind is known in advance.
    let is_model_page = segments.len() >= 2 && segments[0] == "models" && is_id(&segments[1]);
    if is_model_page && host != DEFAULT_HF_HOST {
        civitai_url(host, &segments[1], query)
    } else {
        hf_url(host, &segments)
    }
}

fn civitai_url(host: String, model_id: &str, query: &str) -> Result<Reference, Reason> {
    let mut version_id = None;
    let mut file_id = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("modelVersionId", value)) => version_id = Some(parse_id("version id", value)?),
            Some(("fileId", value)) => file_id = Some(parse_id("file id", value)?),
            _ => {}
        }
    }
    Ok(Reference {
        source: Source::Civitai {
            host,
            model_id: parse_id("model id", model_id)?,
            version_id,
        },
        revision: None,
        path: None,
        file_id,
    })
}

fn hf_url(host: String, segments: &[String]) -> Result<Reference, Reason> {
    const VIEWS: [&str; 3] = ["blob", "resolve", "tree"];
    let Some(first) = segments.first() else {
        return Err("the URL does not name a repository".into());
    };
    if ["datasets", "spaces"].contains(&first.as_str()) {
        return Err(format!(
            "{first} are not supported, only model repositories"
        ));
    }
    // Repositories without an organization have a single part.
    let repo_len = match segments.get(1) {
        Some(second) if !VIEWS.contains(&second.as_str()) => 2,
        _ => 1,
    };
    let (repo, rest) = segments.split_at(repo_len);
    for segment in repo {
        check_repo_segment(segment)?;
    }
    let (revision, path) = match rest {
        [] => (None, None),
        [view, revision] if view == "tree" => (Some(revision.clone()), None),
        [view, _, _, ..] if view == "tree" => {
            return Err("the URL points at a directory; give the repository or a file".into());
        }
        [view, revision, path @ ..] if VIEWS.contains(&view.as_str()) && !path.is_empty() => {
            (Some(revision.clone()), Some(path.join("/")))
        }
        _ => {
            return Err("expected <repository>[/blob/<revision>/<path>]".into());
        }
    };
    Ok(Reference {
        source: Source::HuggingFace {
            host,
            repo: repo.join("/"),
        },
        revision,
        path,
        file_id: None,
    })
}

/// A path segment of a URL, percent-decoded. Segments that would change the
/// meaning of a path built from them are rejected.
fn decode_segment(segment: &str) -> Result<String, Reason> {
    let decoded = percent_decode_str(segment)
        .decode_utf8()
        .map_err(|_| format!("{segment:?} is not valid UTF-8"))?;
    if decoded == "." || decoded == ".." || decoded.contains(['/', '\\', '\0']) {
        return Err(format!("unsafe path segment {segment:?}"));
    }
    Ok(decoded.into_owned())
}

/// Reduces a `host[:port]` to its one spelling (see [`Source`]) and checks
/// that it only has characters that are safe in a directory name.
///
/// `default_port` is the default port of the scheme the host was written
/// with; it is dropped like 443 is.
fn normalize_host(host: &str, default_port: u16) -> Result<String, Reason> {
    let invalid = || format!("invalid host {host:?}");
    let lower = host.to_ascii_lowercase();
    let (name, port) = match lower.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (lower.as_str(), None),
    };
    let name_ok = !name.is_empty()
        && !name.starts_with(['.', '-'])
        && !name.ends_with('.')
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if !name_ok {
        return Err(invalid());
    }
    let port = match port {
        // `parse` alone would also take a sign.
        Some(port) if is_id(port) => Some(port.parse::<u16>().map_err(|_| invalid())?),
        Some(_) => return Err(invalid()),
        None => None,
    };
    match port {
        Some(port) if port != HTTPS_PORT && port != default_port => Ok(format!("{name}:{port}")),
        _ => Ok(name.to_owned()),
    }
}

fn check_repo_segment(segment: &str) -> Result<(), Reason> {
    let ok = !segment.is_empty()
        && !segment.starts_with('.')
        && !segment.contains("..")
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    if ok {
        Ok(())
    } else {
        Err(format!("invalid repository name {segment:?}"))
    }
}

fn is_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

fn parse_id(what: &str, value: &str) -> Result<u64, Reason> {
    if is_id(value) {
        if let Ok(id) = value.parse() {
            return Ok(id);
        }
    }
    Err(format!("invalid {what} {value:?}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Result<Reference, ParseError> {
        Reference::parse(input, &Defaults::default())
    }

    fn hf(host: &str, repo: &str) -> Source {
        Source::HuggingFace {
            host: host.into(),
            repo: repo.into(),
        }
    }

    fn civitai(host: &str, model_id: u64, version_id: Option<u64>) -> Source {
        Source::Civitai {
            host: host.into(),
            model_id,
            version_id,
        }
    }

    #[test]
    fn hf_canonical_forms() {
        let short = parse("hf:example-org/example-model").unwrap();
        assert_eq!(
            short.source,
            hf("huggingface.co", "example-org/example-model")
        );
        assert_eq!(
            (short.revision, short.path, short.file_id),
            (None, None, None)
        );

        let with_host = parse("hf:Hub.Example.com:8080:example-org/example_model.v2").unwrap();
        assert_eq!(
            with_host.source,
            hf("hub.example.com:8080", "example-org/example_model.v2")
        );

        assert_eq!(
            parse("hf:example-model").unwrap().source,
            hf("huggingface.co", "example-model")
        );
    }

    #[test]
    fn civitai_canonical_forms() {
        let pinned = parse("civitai:example.com:12345@67890").unwrap();
        assert_eq!(pinned.source, civitai("example.com", 12345, Some(67890)));

        let latest = parse("civitai:example.com:12345").unwrap();
        assert_eq!(latest.source, civitai("example.com", 12345, None));
    }

    #[test]
    fn civitai_host_comes_from_defaults() {
        assert!(parse("civitai:12345@67890").is_err());

        let defaults = Defaults {
            civitai_host: Some("Example.com".into()),
        };
        let parsed = Reference::parse("civitai:12345@67890", &defaults).unwrap();
        assert_eq!(parsed.source, civitai("example.com", 12345, Some(67890)));
    }

    #[test]
    fn display_is_canonical_and_round_trips() {
        for (input, canonical) in [
            (
                "hf:example-org/example-model",
                "hf:huggingface.co:example-org/example-model",
            ),
            (
                "hf:hub.example.com:example-model",
                "hf:hub.example.com:example-model",
            ),
            (
                "civitai:example.com:12345@67890",
                "civitai:example.com:12345@67890",
            ),
            ("civitai:example.com:12345", "civitai:example.com:12345"),
            (
                "https://example.com/models/12345/some-name?modelVersionId=67890",
                "civitai:example.com:12345@67890",
            ),
        ] {
            let source = parse(input).unwrap().source;
            assert_eq!(source.to_string(), canonical);
            assert_eq!(parse(canonical).unwrap().source, source);
        }
    }

    #[test]
    fn hf_urls() {
        let repo = parse("https://huggingface.co/example-org/example-model").unwrap();
        assert_eq!(
            repo.source,
            hf("huggingface.co", "example-org/example-model")
        );
        assert_eq!((repo.revision, repo.path), (None, None));

        let file = parse(
            "https://huggingface.co/example-org/example-model/blob/main/vae/model%20a.safetensors",
        )
        .unwrap();
        assert_eq!(
            file.source,
            hf("huggingface.co", "example-org/example-model")
        );
        assert_eq!(file.revision.as_deref(), Some("main"));
        assert_eq!(file.path.as_deref(), Some("vae/model a.safetensors"));

        let resolve = parse("HTTPS://Hub.Example.com/example-org/example-model/resolve/v1.0/model.safetensors?download=true#x")
            .unwrap();
        assert_eq!(
            resolve.source,
            hf("hub.example.com", "example-org/example-model")
        );
        assert_eq!(resolve.revision.as_deref(), Some("v1.0"));
        assert_eq!(resolve.path.as_deref(), Some("model.safetensors"));

        let tree = parse("https://huggingface.co/example-org/example-model/tree/dev/").unwrap();
        assert_eq!((tree.revision.as_deref(), tree.path), (Some("dev"), None));

        let no_org =
            parse("https://huggingface.co/example-model/blob/main/model.safetensors").unwrap();
        assert_eq!(no_org.source, hf("huggingface.co", "example-model"));
        assert_eq!(no_org.path.as_deref(), Some("model.safetensors"));
    }

    #[test]
    fn civitai_urls() {
        let latest = parse("https://example.com/models/12345").unwrap();
        assert_eq!(latest.source, civitai("example.com", 12345, None));
        assert_eq!(latest.file_id, None);

        let pinned =
            parse("https://example.com/models/12345/some-name?modelVersionId=67890&fileId=111")
                .unwrap();
        assert_eq!(pinned.source, civitai("example.com", 12345, Some(67890)));
        assert_eq!(pinned.file_id, Some(111));

        let with_port = parse("http://127.0.0.1:8080/models/1?modelVersionId=2").unwrap();
        assert_eq!(with_port.source, civitai("127.0.0.1:8080", 1, Some(2)));
    }

    #[test]
    fn model_page_shape_on_the_default_hub_is_a_repository() {
        for input in [
            "https://huggingface.co/models/12345",
            "https://huggingface.co:443/models/12345",
        ] {
            let parsed = parse(input).unwrap();
            assert_eq!(
                parsed.source,
                hf("huggingface.co", "models/12345"),
                "{input:?}"
            );
        }
    }

    #[test]
    fn a_host_has_one_spelling() {
        for (input, host) in [
            (
                "https://hub.example.com:443/example-model",
                "hub.example.com",
            ),
            (
                "https://hub.example.com:0443/example-model",
                "hub.example.com",
            ),
            ("http://hub.example.com:80/example-model", "hub.example.com"),
            (
                "http://hub.example.com:443/example-model",
                "hub.example.com",
            ),
            (
                "https://hub.example.com:80/example-model",
                "hub.example.com:80",
            ),
            (
                "https://hub.example.com:08080/example-model",
                "hub.example.com:8080",
            ),
            ("hf:hub.example.com:443:example-model", "hub.example.com"),
            ("hf:hub.example.com:80:example-model", "hub.example.com:80"),
            (
                "hf:hub.example.com:0080:example-model",
                "hub.example.com:80",
            ),
        ] {
            assert_eq!(
                parse(input).unwrap().source,
                hf(host, "example-model"),
                "{input:?}"
            );
        }

        assert_eq!(
            parse("civitai:example.com:443:12345").unwrap().source,
            civitai("example.com", 12345, None)
        );
        let defaults = Defaults {
            civitai_host: Some("example.com:443".into()),
        };
        assert_eq!(
            Reference::parse("civitai:12345", &defaults).unwrap().source,
            civitai("example.com", 12345, None)
        );
    }

    #[test]
    fn rejects_malformed_input() {
        for input in [
            "",
            "example-org/example-model",
            "ftp://example.com/models/1",
            "hf:",
            "hf:a/b/c",
            "hf:../example-model",
            "hf:example-org/.hidden",
            "hf:example-org/has space",
            "hf:bad host:example-org/example-model",
            "hf:example.com:port:example-org/example-model",
            "hf:example.com.:example-org/example-model",
            "hf:example.com:65536:example-org/example-model",
            "hf:example.com:+80:example-org/example-model",
            "https://example.com./example-org/example-model",
            "https://example.com:/example-org/example-model",
            "civitai:example.com:",
            "civitai:example.com:abc",
            "civitai:example.com:1@",
            "civitai:example.com:1@x",
            "civitai:example.com:-1",
            "civitai:example.com:99999999999999999999999",
            "civitai:..:1",
            "https://",
            "https://huggingface.co/",
            "https://user@huggingface.co/example-org/example-model",
            "https://huggingface.co/datasets/example-org/example-data",
            "https://huggingface.co/spaces/example-org/example-space",
            "https://huggingface.co/example-org/example-model/blob/main",
            "https://huggingface.co/example-org/example-model/tree/main/subdir",
            "https://huggingface.co/example-org/example-model/discussions/1",
            "https://huggingface.co/example-org/example-model/blob/main/a/%2e%2e/b",
            "https://huggingface.co/example-org/example-model/blob/main/a%2Fb",
            "https://huggingface.co/example-org/example-model/blob/main/%ff",
            "https://example.com/models/1?modelVersionId=x",
        ] {
            assert!(parse(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn error_names_the_input() {
        let err = parse("hf:a/b/c").unwrap_err();
        assert!(
            err.to_string().starts_with("invalid source \"hf:a/b/c\": "),
            "{err}"
        );
    }
}
