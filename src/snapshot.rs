//! The files of a mirror's public tree that describe a model: what a mirror
//! writes and a node reads.
//!
//! Paths here are relative to the root of the public tree and use `/`, so
//! that they serve both as file paths and as URL paths.

use serde::{Deserialize, Serialize};

use crate::hash::Sha256;
use crate::source::Source;
use crate::upstream::encode_segment;

/// The version of the snapshot format this program writes and reads.
pub const SCHEMA: u32 = 1;

/// A model at one revision: every file the upstream lists for it, and which
/// of them the mirror holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Version of the format.
    pub schema: u32,
    /// The source in canonical form, without a revision.
    pub source: String,
    /// The commit of a repository or the id of a model version.
    pub revision: String,
    /// The files, ordered by path and file id.
    pub files: Vec<SnapshotFile>,
}

/// A file of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotFile {
    /// Path in the repository, or the file name where files have ids.
    pub path: String,
    /// Id of the file, where the upstream gives files ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<u64>,
    /// Size in bytes, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Digest, if the upstream announced it or the mirror holds the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<Sha256>,
    /// What the file is. Only `file` for now.
    pub kind: String,
    /// Whether the upstream marks this as the main file. Left out unless
    /// true.
    #[serde(default, skip_serializing_if = "is_false")]
    pub primary: bool,
    /// Whether the mirror holds the content. Left out unless false.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub fetched: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(value: &bool) -> bool {
    *value
}

fn yes() -> bool {
    true
}

/// The directory of a source below `snapshots/` and `metadata/`.
///
/// A port in the host is separated by `_`, which no host name contains,
/// because `:` is not allowed in file names everywhere.
#[must_use]
pub fn source_dir(source: &Source) -> String {
    match source {
        Source::HuggingFace { host, repo } => format!("hf/{}/{repo}", host.replace(':', "_")),
        Source::Civitai { host, model_id, .. } => {
            format!("civitai/{}/{model_id}", host.replace(':', "_"))
        }
    }
}

/// The path of the snapshot of `source` at `revision`.
#[must_use]
pub fn snapshot_path(source: &Source, revision: &str) -> String {
    format!(
        "snapshots/{}/{}.json",
        source_dir(source),
        encode_segment(revision)
    )
}

/// The path of the file telling which revision `name` stands for: a branch
/// or tag of a repository, or `latest` for a model.
#[must_use]
pub fn ref_path(source: &Source, name: &str) -> String {
    let dir = source_dir(source);
    let name = encode_segment(name);
    match source {
        Source::HuggingFace { .. } => format!("snapshots/{dir}/refs/{name}"),
        Source::Civitai { .. } => format!("snapshots/{dir}/{name}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn hf(host: &str) -> Source {
        Source::HuggingFace {
            host: host.into(),
            repo: "example-org/example-model".into(),
        }
    }

    fn civitai() -> Source {
        Source::Civitai {
            host: "example.com".into(),
            model_id: 12345,
            version_id: Some(67890),
        }
    }

    #[test]
    fn paths_follow_the_layout_of_the_public_tree() {
        assert_eq!(
            snapshot_path(&hf("huggingface.co"), "0123abc"),
            "snapshots/hf/huggingface.co/example-org/example-model/0123abc.json"
        );
        assert_eq!(
            ref_path(&hf("hub.example.com:8443"), "refs/pr/1"),
            "snapshots/hf/hub.example.com_8443/example-org/example-model/refs/refs%2Fpr%2F1"
        );
        assert_eq!(
            snapshot_path(&civitai(), "67890"),
            "snapshots/civitai/example.com/12345/67890.json"
        );
        assert_eq!(
            ref_path(&civitai(), "latest"),
            "snapshots/civitai/example.com/12345/latest"
        );
    }

    #[test]
    fn optional_fields_are_left_out_and_default_when_read() {
        let snapshot = Snapshot {
            schema: SCHEMA,
            source: "hf:huggingface.co:example-org/example-model".into(),
            revision: "0123abc".into(),
            files: vec![
                SnapshotFile {
                    path: "a.json".into(),
                    file_id: None,
                    size: None,
                    sha256: None,
                    kind: "file".into(),
                    primary: false,
                    fetched: true,
                },
                SnapshotFile {
                    path: "b.bin".into(),
                    file_id: Some(111),
                    size: Some(3),
                    sha256: Some(Sha256::of(b"abc")),
                    kind: "file".into(),
                    primary: true,
                    fetched: false,
                },
            ],
        };

        let json = serde_json::to_value(&snapshot).unwrap();

        assert_eq!(
            json["files"],
            serde_json::json!([
                {"path": "a.json", "kind": "file"},
                {"path": "b.bin", "file_id": 111, "size": 3,
                 "sha256": Sha256::of(b"abc").to_string(),
                 "kind": "file", "primary": true, "fetched": false},
            ])
        );
        assert_eq!(serde_json::from_value::<Snapshot>(json).unwrap(), snapshot);
    }

    #[test]
    fn a_malformed_digest_is_rejected() {
        let json = r#"{"schema": 1, "source": "s", "revision": "r",
                       "files": [{"path": "a", "kind": "file", "sha256": "abc"}]}"#;
        assert!(serde_json::from_str::<Snapshot>(json).is_err());
    }
}
