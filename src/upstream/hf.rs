//! Model repositories on a Hugging Face compatible hub.

use serde::Deserialize;

use super::{Endpoint, encode_path, encode_segment};
use crate::download::Client;
use crate::error::{Error, Result};
use crate::fsutil::check_relative;
use crate::hash::Sha256;

/// A hub.
#[derive(Debug, Clone)]
pub struct Hub {
    endpoint: Endpoint,
}

/// A repository at one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    /// The commit the requested revision stands for.
    pub commit: String,
    /// Every file of the repository at that commit.
    pub files: Vec<RepoFile>,
    /// The response of the hub, as received.
    pub raw: Vec<u8>,
}

/// A file of a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFile {
    /// Path from the root of the repository.
    pub path: String,
    /// Size in bytes, if the hub tells.
    pub size: Option<u64>,
    /// Digest of the content. The hub only tells it for files in large file
    /// storage.
    pub sha256: Option<Sha256>,
}

#[derive(Deserialize)]
struct ModelInfo {
    sha: String,
    #[serde(default)]
    siblings: Vec<Sibling>,
}

#[derive(Deserialize)]
struct Sibling {
    rfilename: String,
    size: Option<u64>,
    lfs: Option<Lfs>,
}

#[derive(Deserialize)]
struct Lfs {
    sha256: String,
    size: Option<u64>,
}

impl Hub {
    /// The hub at `base_url`, such as `https://<host>`. The token, if any,
    /// is only sent to URLs below `base_url`.
    #[must_use]
    pub fn new(base_url: &str, token: Option<&str>) -> Self {
        Self {
            endpoint: Endpoint::new(base_url, token),
        }
    }

    /// Asks the hub which commit `revision` stands for and which files the
    /// repository has there.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or the response names a commit
    /// or a file path that cannot be used.
    pub fn repo_info(&self, http: &Client, repo: &str, revision: &str) -> Result<RepoInfo> {
        let url = format!(
            "{}/api/models/{}/revision/{}?blobs=true",
            self.endpoint.base_url,
            encode_path(repo),
            encode_segment(revision)
        );
        let raw = http.get(&url, &self.endpoint.headers_for(&url))?;
        let invalid = |reason: String| Error::Upstream {
            url: url.clone(),
            reason,
        };
        let info: ModelInfo = serde_json::from_slice(&raw).map_err(|e| invalid(e.to_string()))?;

        let is_commit = info.sha.len() == 40
            && info
                .sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !is_commit {
            return Err(invalid(format!("invalid commit {:?}", info.sha)));
        }
        let files =
            info.siblings
                .into_iter()
                .map(|sibling| {
                    check_relative(&sibling.rfilename).map_err(&invalid)?;
                    let (size, sha256) = match sibling.lfs {
                        Some(lfs) => {
                            let digest = lfs.sha256.parse().map_err(
                                |e: crate::hash::ParseSha256Error| invalid(e.to_string()),
                            )?;
                            (lfs.size.or(sibling.size), Some(digest))
                        }
                        None => (sibling.size, None),
                    };
                    Ok(RepoFile {
                        path: sibling.rfilename,
                        size,
                        sha256,
                    })
                })
                .collect::<Result<_>>()?;
        Ok(RepoInfo {
            commit: info.sha,
            files,
            raw,
        })
    }

    /// The URL the content of a file is downloaded from.
    #[must_use]
    pub fn file_url(&self, repo: &str, commit: &str, path: &str) -> String {
        format!(
            "{}/{}/resolve/{}/{}",
            self.endpoint.base_url,
            encode_path(repo),
            encode_segment(commit),
            encode_path(path)
        )
    }

    /// The headers to send with a request to `url`.
    #[must_use]
    pub fn headers_for(&self, url: &str) -> Vec<(&str, &str)> {
        self.endpoint.headers_for(url)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::download::RetryPolicy;
    use crate::testutil::{Response, TestServer};

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn http() -> Client {
        Client::new(RetryPolicy {
            max_failures: 1,
            ..RetryPolicy::default()
        })
    }

    fn serve(body: String) -> TestServer {
        TestServer::start(move |_| Response::ok(body.as_bytes()))
    }

    #[test]
    fn repo_info_lists_files_with_what_the_hub_knows() {
        let digest = Sha256::of(b"weights");
        let body = format!(
            r#"{{"id": "example-org/example-model", "sha": "{COMMIT}", "siblings": [
                {{"rfilename": "config.json", "blobId": "x", "size": 12}},
                {{"rfilename": "vae/model.safetensors", "size": 7,
                  "lfs": {{"sha256": "{digest}", "size": 7, "pointerSize": 131}}}},
                {{"rfilename": "README.md"}}
            ]}}"#
        );
        let server = serve(body.clone());
        let hub = Hub::new(&server.url(""), Some("secret"));

        let info = hub
            .repo_info(&http(), "example-org/example-model", "refs/pr/1")
            .unwrap();

        assert_eq!(info.commit, COMMIT);
        assert_eq!(info.raw, body.as_bytes());
        assert_eq!(
            info.files,
            [
                RepoFile {
                    path: "config.json".into(),
                    size: Some(12),
                    sha256: None
                },
                RepoFile {
                    path: "vae/model.safetensors".into(),
                    size: Some(7),
                    sha256: Some(digest)
                },
                RepoFile {
                    path: "README.md".into(),
                    size: None,
                    sha256: None
                },
            ]
        );
        let requests = server.requests();
        assert_eq!(
            requests[0].path,
            "/api/models/example-org/example-model/revision/refs%2Fpr%2F1?blobs=true"
        );
        assert_eq!(requests[0].header("authorization"), Some("Bearer secret"));
    }

    #[test]
    fn file_urls_name_the_commit_and_escape_the_path() {
        let hub = Hub::new("https://hub.example.com/", None);
        assert_eq!(
            hub.file_url("example-org/example-model", COMMIT, "dir a/model#1.bin"),
            format!(
                "https://hub.example.com/example-org/example-model/resolve/{COMMIT}/dir%20a/model%231.bin"
            )
        );
    }

    #[test]
    fn unusable_responses_are_rejected() {
        let digest = Sha256::of(b"x");
        for body in [
            "not json".to_owned(),
            r#"{"siblings": []}"#.to_owned(),
            r#"{"sha": "main", "siblings": []}"#.to_owned(),
            format!(r#"{{"sha": "{}", "siblings": []}}"#, COMMIT.to_uppercase()),
            format!(r#"{{"sha": "{COMMIT}", "siblings": [{{"rfilename": "../escape"}}]}}"#),
            format!(r#"{{"sha": "{COMMIT}", "siblings": [{{"rfilename": "/etc/passwd"}}]}}"#),
            format!(
                r#"{{"sha": "{COMMIT}", "siblings": [{{"rfilename": "a", "lfs": {{"sha256": "abc"}}}}]}}"#
            ),
            format!(
                r#"{{"sha": "{COMMIT}", "siblings": [{{"rfilename": "a", "lfs": {{"size": 1}}}}]}}"#
            ),
            format!(
                r#"{{"sha": "{COMMIT}", "siblings": [{{"rfilename": 1, "lfs": {{"sha256": "{digest}"}}}}]}}"#
            ),
        ] {
            let server = serve(body.clone());
            let hub = Hub::new(&server.url(""), None);
            let result = hub.repo_info(&http(), "example-org/example-model", "main");
            assert!(matches!(result, Err(Error::Upstream { .. })), "{body}");
        }
    }

    #[test]
    fn a_refused_request_is_reported_with_its_status() {
        let server = TestServer::start(|_| Response::status(404));
        let hub = Hub::new(&server.url(""), None);

        let error = hub
            .repo_info(&http(), "example-org/missing", "main")
            .unwrap_err();

        assert!(error.to_string().ends_with("HTTP status 404"), "{error}");
    }
}
