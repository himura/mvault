//! Models on a Civitai compatible site.

use serde::Deserialize;

use super::Endpoint;
use crate::download::Client;
use crate::error::{Error, Result};
use crate::fsutil::check_relative;
use crate::hash::Sha256;

/// A site.
#[derive(Debug, Clone)]
pub struct Site {
    endpoint: Endpoint,
}

/// A version of a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// Id of the model the version belongs to.
    pub model_id: u64,
    /// Id of the version.
    pub version_id: u64,
    /// Every file of the version.
    pub files: Vec<VersionFile>,
    /// The response of the site, as received.
    pub raw: Vec<u8>,
}

/// A file of a model version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionFile {
    /// Id of the file.
    pub id: u64,
    /// File name. Two files of a version may have the same one.
    pub name: String,
    /// Digest of the content, if the site tells.
    pub sha256: Option<Sha256>,
    /// Whether this is the model itself, as opposed to training data,
    /// configuration or another format of it.
    pub primary: bool,
    /// The URL the content is downloaded from.
    pub download_url: String,
}

#[derive(Deserialize)]
struct ModelBody {
    id: u64,
    #[serde(default, rename = "modelVersions")]
    versions: Vec<ModelVersionBody>,
}

#[derive(Deserialize)]
struct ModelVersionBody {
    id: u64,
}

#[derive(Deserialize)]
struct VersionBody {
    id: u64,
    #[serde(rename = "modelId")]
    model_id: u64,
    #[serde(default)]
    files: Vec<FileBody>,
}

#[derive(Deserialize)]
struct FileBody {
    id: u64,
    name: String,
    #[serde(default)]
    primary: Option<bool>,
    #[serde(default)]
    hashes: Option<Hashes>,
    #[serde(rename = "downloadUrl")]
    download_url: String,
}

#[derive(Deserialize)]
struct Hashes {
    #[serde(rename = "SHA256")]
    sha256: Option<String>,
}

impl Site {
    /// The site at `base_url`, such as `https://<host>`. The token, if any,
    /// is only sent to URLs below `base_url`.
    #[must_use]
    pub fn new(base_url: &str, token: Option<&str>) -> Self {
        Self {
            endpoint: Endpoint::new(base_url, token),
        }
    }

    /// Fetches the description of a model and returns the id of its latest
    /// version together with the response as received.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, the response is about another
    /// model, or the model has no versions.
    pub fn model(&self, http: &Client, model_id: u64) -> Result<(Option<u64>, Vec<u8>)> {
        let url = format!("{}/api/v1/models/{model_id}", self.endpoint.base_url);
        let raw = http.get(&url, &self.endpoint.headers_for(&url))?;
        let invalid = |reason: String| Error::Upstream {
            url: url.clone(),
            reason,
        };
        let model: ModelBody = serde_json::from_slice(&raw).map_err(|e| invalid(e.to_string()))?;
        if model.id != model_id {
            return Err(invalid(format!("the response is about model {}", model.id)));
        }
        // The site lists versions newest first.
        Ok((model.versions.first().map(|version| version.id), raw))
    }

    /// Fetches the description of a model version.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, the response is about another
    /// version, or it names a file that cannot be used.
    pub fn version(&self, http: &Client, version_id: u64) -> Result<Version> {
        let url = format!(
            "{}/api/v1/model-versions/{version_id}",
            self.endpoint.base_url
        );
        let raw = http.get(&url, &self.endpoint.headers_for(&url))?;
        let invalid = |reason: String| Error::Upstream {
            url: url.clone(),
            reason,
        };
        let version: VersionBody =
            serde_json::from_slice(&raw).map_err(|e| invalid(e.to_string()))?;
        if version.id != version_id {
            return Err(invalid(format!(
                "the response is about version {}",
                version.id
            )));
        }

        // The model itself is the one file marked primary; a version with a
        // single file needs no mark.
        let marked: Vec<u64> = version
            .files
            .iter()
            .filter(|file| file.primary == Some(true))
            .map(|file| file.id)
            .collect();
        let primary = match (marked.as_slice(), version.files.as_slice()) {
            ([id], _) => Some(*id),
            ([], [only]) => Some(only.id),
            _ => None,
        };

        let files = version
            .files
            .into_iter()
            .map(|file| {
                if file.name.contains('/') {
                    return Err(invalid(format!("unsafe file name {:?}", file.name)));
                }
                check_relative(&file.name).map_err(&invalid)?;
                let on_site = self.endpoint.contains(&file.download_url);
                if !on_site && !file.download_url.starts_with("https://") {
                    return Err(invalid(format!(
                        "download URL {:?} is not https",
                        file.download_url
                    )));
                }
                let sha256 = file
                    .hashes
                    .and_then(|hashes| hashes.sha256)
                    .map(|hex| hex.parse::<Sha256>())
                    .transpose()
                    .map_err(|e| invalid(e.to_string()))?;
                Ok(VersionFile {
                    primary: primary == Some(file.id),
                    id: file.id,
                    name: file.name,
                    sha256,
                    download_url: file.download_url,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Version {
            model_id: version.model_id,
            version_id: version.id,
            files,
            raw,
        })
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

    fn http() -> Client {
        Client::new(RetryPolicy {
            max_failures: 1,
            ..RetryPolicy::default()
        })
    }

    fn serve(body: String) -> TestServer {
        TestServer::start(move |_| Response::ok(body.as_bytes()))
    }

    fn file(id: u64, name: &str, extra: &str) -> String {
        format!(
            r#"{{"id": {id}, "name": "{name}", "downloadUrl": "https://example.com/api/download/models/{id}"{extra}}}"#
        )
    }

    fn version_body(files: &[String]) -> String {
        format!(
            r#"{{"id": 67890, "modelId": 12345, "files": [{}]}}"#,
            files.join(",")
        )
    }

    #[test]
    fn model_gives_the_latest_version() {
        let body = r#"{"id": 12345, "name": "x", "modelVersions": [{"id": 67890}, {"id": 67000}]}"#;
        let server = serve(body.to_owned());
        let site = Site::new(&server.url(""), Some("secret"));

        let (latest, raw) = site.model(&http(), 12345).unwrap();

        assert_eq!(latest, Some(67890));
        assert_eq!(raw, body.as_bytes());
        let requests = server.requests();
        assert_eq!(requests[0].path, "/api/v1/models/12345");
        assert_eq!(requests[0].header("authorization"), Some("Bearer secret"));
    }

    #[test]
    fn model_without_versions_has_no_latest() {
        let server = serve(r#"{"id": 12345}"#.to_owned());
        let site = Site::new(&server.url(""), None);
        assert_eq!(site.model(&http(), 12345).unwrap().0, None);
    }

    #[test]
    fn model_must_be_the_one_asked_for() {
        let server = serve(r#"{"id": 999, "modelVersions": []}"#.to_owned());
        let site = Site::new(&server.url(""), None);
        assert!(matches!(
            site.model(&http(), 12345),
            Err(Error::Upstream { .. })
        ));
    }

    #[test]
    fn version_lists_files_and_finds_the_primary_one() {
        let digest = Sha256::of(b"weights");
        let upper = digest.to_string().to_uppercase();
        let body = version_body(&[
            file(
                111,
                "model.safetensors",
                &format!(r#", "primary": true, "hashes": {{"SHA256": "{upper}", "CRC32": "x"}}"#),
            ),
            file(222, "model.safetensors", r#", "hashes": {}"#),
            file(333, "training.zip", r#", "primary": false"#),
        ]);
        let server = serve(body.clone());
        let site = Site::new(&server.url(""), None);

        let version = site.version(&http(), 67890).unwrap();

        assert_eq!((version.model_id, version.version_id), (12345, 67890));
        assert_eq!(version.raw, body.as_bytes());
        let summary: Vec<_> = version
            .files
            .iter()
            .map(|f| (f.id, f.name.as_str(), f.sha256, f.primary))
            .collect();
        assert_eq!(
            summary,
            [
                (111, "model.safetensors", Some(digest), true),
                (222, "model.safetensors", None, false),
                (333, "training.zip", None, false),
            ]
        );
        assert_eq!(
            version.files[0].download_url,
            "https://example.com/api/download/models/111"
        );
        assert_eq!(server.requests()[0].path, "/api/v1/model-versions/67890");
    }

    #[test]
    fn a_single_file_is_primary_without_a_mark() {
        let server = serve(version_body(&[file(111, "model.safetensors", "")]));
        let site = Site::new(&server.url(""), None);
        assert!(site.version(&http(), 67890).unwrap().files[0].primary);
    }

    #[test]
    fn no_file_is_primary_when_the_marks_are_ambiguous() {
        for files in [
            vec![file(111, "a.bin", ""), file(222, "b.bin", "")],
            vec![
                file(111, "a.bin", r#", "primary": true"#),
                file(222, "b.bin", r#", "primary": true"#),
            ],
        ] {
            let server = serve(version_body(&files));
            let site = Site::new(&server.url(""), None);
            let version = site.version(&http(), 67890).unwrap();
            assert!(version.files.iter().all(|f| !f.primary));
        }
    }

    #[test]
    fn unusable_versions_are_rejected() {
        for body in [
            "not json".to_owned(),
            r#"{"id": 67890}"#.to_owned(),
            r#"{"id": 1, "modelId": 12345, "files": []}"#.to_owned(),
            version_body(&[file(111, "../escape", "")]),
            version_body(&[file(111, "dir/name", "")]),
            version_body(&[file(111, "", "")]),
            version_body(&[file(111, "a.bin", r#", "hashes": {"SHA256": "abc"}"#)]),
            version_body(&[
                r#"{"id": 111, "name": "a.bin", "downloadUrl": "http://example.com/x"}"#.to_owned(),
            ]),
            version_body(&[r#"{"id": 111, "name": "a.bin"}"#.to_owned()]),
        ] {
            let server = serve(body.clone());
            let site = Site::new(&server.url(""), None);
            let result = site.version(&http(), 67890);
            assert!(matches!(result, Err(Error::Upstream { .. })), "{body}");
        }
    }

    #[test]
    fn a_download_url_on_the_site_itself_is_accepted_and_authorized() {
        let server = TestServer::start(|_| Response::status(500));
        let base = server.url("");
        let site = Site::new(&base, Some("secret"));
        let on_site = format!("{base}/api/download/models/111");

        assert_eq!(
            site.headers_for(&on_site),
            [("Authorization", "Bearer secret")]
        );
        assert_eq!(
            site.headers_for("https://cdn.example.net/file"),
            [] as [(&str, &str); 0]
        );
    }
}
