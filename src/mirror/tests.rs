#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::*;
use crate::download::RetryPolicy;
use crate::testutil::{Response, TestServer};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const REPO: &str = "example-org/example-model";
const HOST: &str = "hub.example.com";

fn http() -> Client {
    Client::new(RetryPolicy {
        max_failures: 1,
        ..RetryPolicy::default()
    })
}

/// A server answering each path with a fixed body; the table can be changed
/// while it runs.
struct Upstream {
    server: TestServer,
    routes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl Upstream {
    fn start() -> Self {
        let routes: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let server = TestServer::start({
            let routes = Arc::clone(&routes);
            move |request| match routes.lock().unwrap().get(&request.path) {
                Some(body) => Response::file(request, body),
                None => Response::status(404),
            }
        });
        Self { server, routes }
    }

    fn route(&self, path: &str, body: &[u8]) {
        self.routes
            .lock()
            .unwrap()
            .insert(path.to_owned(), body.to_vec());
    }

    fn base(&self) -> String {
        self.server.url("")
    }

    /// The paths requested so far, without the API calls.
    fn downloads(&self) -> Vec<String> {
        self.server
            .requests()
            .into_iter()
            .map(|request| request.path)
            .filter(|path| !path.starts_with("/api/v1/") && !path.starts_with("/api/models/"))
            .collect()
    }
}

/// A hub with one repository: a small file without a digest, a large file
/// with one, and a sample under `assets/`.
fn hub_with_repo(upstream: &Upstream, commit: &str, revision: &str) {
    let weights = Sha256::of(b"weights");
    let sample = Sha256::of(b"sample");
    upstream.route(
        &format!("/api/models/{REPO}/revision/{revision}?blobs=true"),
        format!(
            r#"{{"sha": "{commit}", "siblings": [
                {{"rfilename": "config.json", "size": 2}},
                {{"rfilename": "vae/model.safetensors", "size": 7, "lfs": {{"sha256": "{weights}", "size": 7}}}},
                {{"rfilename": "assets/sample.mp4", "size": 6, "lfs": {{"sha256": "{sample}", "size": 6}}}}
            ]}}"#
        )
        .as_bytes(),
    );
    upstream.route(&format!("/{REPO}/resolve/{commit}/config.json"), b"{}");
    upstream.route(
        &format!("/{REPO}/resolve/{commit}/vae/model.safetensors"),
        b"weights",
    );
    upstream.route(
        &format!("/{REPO}/resolve/{commit}/assets/sample.mp4"),
        b"sample",
    );
}

fn pull_hf(
    mirror: &mut Mirror,
    upstream: &Upstream,
    revision: &str,
    excludes: &[&str],
) -> Result<Pulled> {
    let hub = Hub::new(&upstream.base(), None);
    let excludes = Excludes::new(excludes, &[] as &[&str]).unwrap();
    let pull = HfPull {
        hub: &hub,
        host: HOST,
        repo: REPO,
        revision,
        excludes: &excludes,
    };
    mirror.pull_hf(&http(), &pull, &mut |_| {})
}

fn blob(mirror: &Mirror, content: &[u8]) -> Option<Vec<u8>> {
    fs::read(mirror.store.blob_path(&Sha256::of(content))).ok()
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn hf_snapshot(mirror: &Mirror, commit: &str) -> serde_json::Value {
    read_json(
        &mirror
            .public_dir()
            .join(format!("snapshots/hf/{HOST}/{REPO}/{commit}.json")),
    )
}

#[test]
fn hf_pull_fetches_files_and_writes_the_public_tree() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;

    let pulled = pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap();

    assert_eq!(
        pulled,
        Pulled {
            source: format!("hf:{HOST}:{REPO}"),
            revision: COMMIT.to_owned(),
            downloaded: 2,
            reused: 0,
            present: 0,
            skipped: 1,
        }
    );
    assert_eq!(blob(&mirror, b"{}").unwrap(), b"{}");
    assert_eq!(blob(&mirror, b"weights").unwrap(), b"weights");
    assert_eq!(blob(&mirror, b"sample"), None);

    let public = mirror.public_dir();
    assert_eq!(
        hf_snapshot(&mirror, COMMIT),
        serde_json::json!({
            "schema": 1,
            "source": format!("hf:{HOST}:{REPO}"),
            "revision": COMMIT,
            "files": [
                {"path": "assets/sample.mp4", "size": 6, "sha256": Sha256::of(b"sample").to_string(),
                 "kind": "file", "fetched": false},
                {"path": "config.json", "size": 2, "sha256": Sha256::of(b"{}").to_string(), "kind": "file"},
                {"path": "vae/model.safetensors", "size": 7, "sha256": Sha256::of(b"weights").to_string(),
                 "kind": "file"},
            ],
        })
    );
    assert_eq!(
        fs::read_to_string(public.join(format!("snapshots/hf/{HOST}/{REPO}/refs/main"))).unwrap(),
        format!("{COMMIT}\n")
    );
    let metadata = read_json(&public.join(format!("metadata/hf/{HOST}/{REPO}/{COMMIT}.json")));
    assert_eq!(metadata["sha"], COMMIT);
    // Nothing unfinished is left behind, and nothing private is served.
    assert!(!public.join("mvault.db").exists());
    let leftovers: Vec<_> = fs::read_dir(dir.path().join("private/tmp"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| Path::new(name).extension().is_none())
        .collect();
    assert_eq!(leftovers, Vec::<String>::new());
}

#[test]
fn hf_pull_again_downloads_nothing_and_adds_what_was_excluded() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap();
    let before = upstream.downloads().len();

    let again = pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap();
    assert_eq!((again.downloaded, again.present, again.skipped), (0, 2, 1));
    assert_eq!(upstream.downloads().len(), before);

    // A narrower pull does not forget what an earlier one fetched.
    let narrower = pull_hf(&mut mirror, &upstream, "main", &["assets", "*.json"]).unwrap();
    // What is not fetched is counted in the snapshot: config.json, left out
    // this time, was fetched before.
    assert_eq!((narrower.present, narrower.skipped), (1, 1));
    assert_eq!(
        hf_snapshot(&mirror, COMMIT)["files"][1].get("fetched"),
        None
    );

    let all = pull_hf(&mut mirror, &upstream, "main", &[]).unwrap();
    assert_eq!((all.downloaded, all.present, all.skipped), (1, 2, 0));
    assert_eq!(blob(&mirror, b"sample").unwrap(), b"sample");
    assert_eq!(
        hf_snapshot(&mirror, COMMIT)["files"][0].get("fetched"),
        None
    );
}

#[test]
fn hf_pull_of_a_new_commit_reuses_blobs_and_moves_the_ref() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap();

    let next = "89abcdef0123456789abcdef0123456789abcdef";
    hub_with_repo(&upstream, next, "main");
    let pulled = pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap();

    // The file with a digest is recognized; the one without is fetched again.
    assert_eq!((pulled.downloaded, pulled.reused), (1, 1));
    let refs = mirror
        .public_dir()
        .join(format!("snapshots/hf/{HOST}/{REPO}/refs/main"));
    assert_eq!(fs::read_to_string(refs).unwrap(), format!("{next}\n"));
    assert_eq!(hf_snapshot(&mirror, COMMIT)["revision"], COMMIT);
    assert_eq!(hf_snapshot(&mirror, next)["revision"], next);
}

#[test]
fn hf_pull_that_fails_leaves_the_ref_where_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    pull_hf(&mut mirror, &upstream, "main", &[]).unwrap();

    // The upstream moves on, and a file of the new commit cannot be fetched.
    let newer = "1111111111111111111111111111111111111111";
    upstream.route(
        &format!("/api/models/{REPO}/revision/main?blobs=true"),
        format!(
            r#"{{"sha": "{newer}", "siblings": [
                {{"rfilename": "config.json", "size": 2}},
                {{"rfilename": "missing.bin", "size": 5}}
            ]}}"#
        )
        .as_bytes(),
    );
    upstream.route(&format!("/{REPO}/resolve/{newer}/config.json"), b"{}");
    let main = mirror
        .public_dir()
        .join(format!("snapshots/hf/{HOST}/{REPO}/refs/main"));

    assert!(pull_hf(&mut mirror, &upstream, "main", &[]).is_err());

    assert_eq!(fs::read_to_string(&main).unwrap(), format!("{COMMIT}\n"));
    // What was fetched is still published.
    let files = &hf_snapshot(&mirror, newer)["files"];
    assert_eq!(files[0]["path"], "config.json");
    assert_eq!(files[0].get("fetched"), None);
    assert_eq!(files[1]["fetched"], false);

    // Once the file can be fetched, the ref follows.
    upstream.route(&format!("/{REPO}/resolve/{newer}/missing.bin"), b"found");
    pull_hf(&mut mirror, &upstream, "main", &[]).unwrap();
    assert_eq!(fs::read_to_string(&main).unwrap(), format!("{newer}\n"));
}

#[test]
fn hf_pull_by_commit_writes_no_ref() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, COMMIT);
    let mut mirror = Mirror::create(dir.path()).unwrap().0;

    pull_hf(&mut mirror, &upstream, COMMIT, &[]).unwrap();

    let refs = mirror
        .public_dir()
        .join(format!("snapshots/hf/{HOST}/{REPO}/refs"));
    assert!(!refs.exists());
}

#[test]
fn hf_pull_rejects_content_with_another_digest() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    upstream.route(
        &format!("/{REPO}/resolve/{COMMIT}/vae/model.safetensors"),
        b"tampered",
    );
    let mut mirror = Mirror::create(dir.path()).unwrap().0;

    let error = pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap_err();

    assert!(error.to_string().contains("expected sha256"), "{error}");
    assert_eq!(blob(&mirror, b"tampered"), None);
    // The file fetched before the failure is served; the bad one is not, and
    // its partial file is gone so that the next pull starts it afresh.
    let snapshot = hf_snapshot(&mirror, COMMIT);
    assert_eq!(snapshot["files"][1].get("fetched"), None);
    assert_eq!(snapshot["files"][2]["fetched"], false);
    let partial = mirror
        .store
        .partial_path(&Sha256::of(b"weights").to_string())
        .unwrap();
    assert!(!partial.exists());
}

#[test]
fn hf_pull_rejects_content_with_another_size() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    upstream.route(
        &format!("/{REPO}/resolve/{COMMIT}/config.json"),
        b"{\"longer\": true}",
    );
    let mut mirror = Mirror::create(dir.path()).unwrap().0;

    let error = pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap_err();

    assert!(error.to_string().contains("expected 2 bytes"), "{error}");
}

#[test]
fn hf_pull_does_not_continue_a_partial_file_without_a_digest() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    let config = FileEntry {
        path: "config.json".into(),
        file_id: None,
        size: Some(2),
        sha256: None,
        primary: false,
        fetched: false,
    };
    let identity = format!("hf:{HOST}:{REPO}@{COMMIT}");
    let partial = mirror
        .store
        .partial_path(&partial_key(&identity, &config))
        .unwrap();
    fs::write(&partial, b"x").unwrap();

    pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap();

    assert_eq!(blob(&mirror, b"{}").unwrap(), b"{}");
    let ranges: Vec<_> = upstream
        .server
        .requests()
        .iter()
        .filter_map(|request| request.header("range").map(str::to_owned))
        .collect();
    assert_eq!(ranges, Vec::<String>::new());
}

#[test]
fn hf_pull_continues_a_partial_file_with_a_digest() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    let partial = mirror
        .store
        .partial_path(&Sha256::of(b"weights").to_string())
        .unwrap();
    fs::write(&partial, b"wei").unwrap();

    pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap();

    assert_eq!(blob(&mirror, b"weights").unwrap(), b"weights");
    let ranges: Vec<_> = upstream
        .server
        .requests()
        .iter()
        .filter_map(|request| request.header("range").map(str::to_owned))
        .collect();
    assert_eq!(ranges, ["bytes=3-"]);
}

#[test]
fn hf_pull_sends_the_token_to_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    let hub = Hub::new(&upstream.base(), Some("secret"));

    mirror
        .pull_hf(
            &http(),
            &HfPull {
                hub: &hub,
                host: HOST,
                repo: REPO,
                revision: "main",
                excludes: &Excludes::default(),
            },
            &mut |_| {},
        )
        .unwrap();

    let requests = upstream.server.requests();
    assert_eq!(requests.len(), 4);
    for request in requests {
        assert_eq!(
            request.header("authorization"),
            Some("Bearer secret"),
            "{}",
            request.path
        );
    }
}

#[test]
fn hf_pull_reports_events_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    let hub = Hub::new(&upstream.base(), None);
    let excludes = Excludes::new(&["assets"], &[] as &[&str]).unwrap();

    let mut log = Vec::new();
    mirror
        .pull_hf(
            &http(),
            &HfPull {
                hub: &hub,
                host: HOST,
                repo: REPO,
                revision: "main",
                excludes: &excludes,
            },
            &mut |event| match event {
                Event::Started { index, count, path } => {
                    log.push(format!("{index}/{count} {path}"));
                }
                Event::Finished(outcome) => {
                    log.push(format!("{outcome:?}"));
                }
                Event::Failed(message) => log.push(format!("failed: {message}")),
                Event::Progress(_) | Event::Warning(_) => {}
            },
        )
        .unwrap();

    assert_eq!(
        log,
        [
            "1/2 config.json",
            "Downloaded",
            "2/2 vae/model.safetensors",
            "Downloaded"
        ]
    );
}

#[test]
fn a_port_in_the_host_becomes_part_of_the_directory_name() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    let hub = Hub::new(&upstream.base(), None);

    let pulled = mirror
        .pull_hf(
            &http(),
            &HfPull {
                hub: &hub,
                host: "hub.example.com:8443",
                repo: REPO,
                revision: "main",
                excludes: &Excludes::default(),
            },
            &mut |_| {},
        )
        .unwrap();

    assert_eq!(pulled.source, format!("hf:hub.example.com:8443:{REPO}"));
    assert!(
        mirror
            .public_dir()
            .join(format!(
                "snapshots/hf/hub.example.com_8443/{REPO}/{COMMIT}.json"
            ))
            .is_file()
    );
}

const SITE: &str = "example.com";

/// A site with model 12345, whose latest version 67890 has a primary file, a
/// second file of the same name and training data without a digest.
fn site_with_model(upstream: &Upstream) {
    let base = upstream.base();
    let weights = Sha256::of(b"weights").to_string().to_uppercase();
    let pruned = Sha256::of(b"pruned");
    upstream.route(
        "/api/v1/models/12345",
        br#"{"id": 12345, "name": "Example", "modelVersions": [{"id": 67890}, {"id": 67000}]}"#,
    );
    upstream.route(
        "/api/v1/model-versions/67890",
        format!(
            r#"{{"id": 67890, "modelId": 12345, "files": [
                {{"id": 111, "name": "model.safetensors", "primary": true,
                  "hashes": {{"SHA256": "{weights}"}}, "downloadUrl": "{base}/api/download/models/67890"}},
                {{"id": 222, "name": "model.safetensors",
                  "hashes": {{"SHA256": "{pruned}"}}, "downloadUrl": "{base}/api/download/models/67890?type=Pruned"}},
                {{"id": 333, "name": "training.zip", "downloadUrl": "{base}/files/training.zip"}}
            ]}}"#
        )
        .as_bytes(),
    );
    upstream.route("/api/download/models/67890", b"weights");
    upstream.route("/api/download/models/67890?type=Pruned", b"pruned");
    upstream.route("/files/training.zip", b"training");
}

fn pull_civitai(
    mirror: &mut Mirror,
    upstream: &Upstream,
    version_id: Option<u64>,
    selection: &Selection,
) -> Result<Pulled> {
    let site = Site::new(&upstream.base(), Some("secret"));
    let pull = CivitaiPull {
        site: &site,
        host: SITE,
        model_id: 12345,
        version_id,
        selection,
    };
    mirror.pull_civitai(&http(), &pull, &mut |_| {})
}

fn civitai_snapshot(mirror: &Mirror) -> serde_json::Value {
    read_json(
        &mirror
            .public_dir()
            .join(format!("snapshots/civitai/{SITE}/12345/67890.json")),
    )
}

#[test]
fn civitai_pull_of_the_latest_version_fetches_the_primary_file() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    site_with_model(&upstream);
    let mut mirror = Mirror::create(dir.path()).unwrap().0;

    let pulled = pull_civitai(&mut mirror, &upstream, None, &Selection::Primary).unwrap();

    assert_eq!(
        pulled,
        Pulled {
            source: format!("civitai:{SITE}:12345"),
            revision: "67890".to_owned(),
            downloaded: 1,
            reused: 0,
            present: 0,
            skipped: 2,
        }
    );
    assert_eq!(blob(&mirror, b"weights").unwrap(), b"weights");
    assert_eq!(blob(&mirror, b"pruned"), None);
    assert_eq!(
        civitai_snapshot(&mirror),
        serde_json::json!({
            "schema": 1,
            "source": format!("civitai:{SITE}:12345"),
            "revision": "67890",
            "files": [
                {"path": "model.safetensors", "file_id": 111, "size": 7,
                 "sha256": Sha256::of(b"weights").to_string(), "kind": "file", "primary": true},
                {"path": "model.safetensors", "file_id": 222,
                 "sha256": Sha256::of(b"pruned").to_string(), "kind": "file", "fetched": false},
                {"path": "training.zip", "file_id": 333, "kind": "file", "fetched": false},
            ],
        })
    );
    let public = mirror.public_dir();
    assert_eq!(
        fs::read_to_string(public.join(format!("snapshots/civitai/{SITE}/12345/latest"))).unwrap(),
        "67890\n"
    );
    let metadata = public.join(format!("metadata/civitai/{SITE}/12345"));
    assert_eq!(read_json(&metadata.join("model.json"))["name"], "Example");
    assert_eq!(read_json(&metadata.join("67890.json"))["modelId"], 12345);
    assert_eq!(upstream.downloads(), ["/api/download/models/67890"]);
    for request in upstream.server.requests() {
        assert_eq!(request.header("authorization"), Some("Bearer secret"));
    }
}

#[test]
fn civitai_pull_that_fails_does_not_set_latest() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    site_with_model(&upstream);
    upstream.route("/api/download/models/67890?type=Pruned", b"other");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    let latest = mirror
        .public_dir()
        .join(format!("snapshots/civitai/{SITE}/12345/latest"));

    assert!(pull_civitai(&mut mirror, &upstream, None, &Selection::All).is_err());

    assert!(!latest.exists());
    assert_eq!(civitai_snapshot(&mirror)["files"][0].get("fetched"), None);

    upstream.route("/api/download/models/67890?type=Pruned", b"pruned");
    pull_civitai(&mut mirror, &upstream, None, &Selection::All).unwrap();
    assert_eq!(fs::read_to_string(&latest).unwrap(), "67890\n");
}

#[test]
fn civitai_pull_of_a_given_version_does_not_move_latest() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    site_with_model(&upstream);
    let mut mirror = Mirror::create(dir.path()).unwrap().0;

    pull_civitai(&mut mirror, &upstream, Some(67890), &Selection::Primary).unwrap();

    let public = mirror.public_dir();
    assert!(
        !public
            .join(format!("snapshots/civitai/{SITE}/12345/latest"))
            .exists()
    );
    assert!(
        public
            .join(format!("metadata/civitai/{SITE}/12345/model.json"))
            .is_file()
    );
}

#[test]
fn civitai_pull_selects_files_by_id_or_all_of_them() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    site_with_model(&upstream);
    let mut mirror = Mirror::create(dir.path()).unwrap().0;

    let chosen = pull_civitai(
        &mut mirror,
        &upstream,
        Some(67890),
        &Selection::Files(vec![222]),
    )
    .unwrap();
    assert_eq!((chosen.downloaded, chosen.skipped), (1, 2));
    assert_eq!(blob(&mirror, b"pruned").unwrap(), b"pruned");
    assert_eq!(blob(&mirror, b"weights"), None);

    let all = pull_civitai(&mut mirror, &upstream, Some(67890), &Selection::All).unwrap();
    assert_eq!((all.downloaded, all.present, all.skipped), (2, 1, 0));
    assert_eq!(blob(&mirror, b"training").unwrap(), b"training");
    let snapshot = civitai_snapshot(&mirror);
    assert!(
        snapshot["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|file| file.get("fetched").is_none())
    );
    assert_eq!(
        snapshot["files"][2]["sha256"],
        Sha256::of(b"training").to_string()
    );
}

#[test]
fn civitai_pull_rejects_a_selection_that_does_not_fit() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    site_with_model(&upstream);
    let mut mirror = Mirror::create(dir.path()).unwrap().0;

    let unknown = pull_civitai(
        &mut mirror,
        &upstream,
        Some(67890),
        &Selection::Files(vec![999]),
    )
    .unwrap_err();
    assert!(
        unknown
            .to_string()
            .contains("has no file 999 (111: model.safetensors"),
        "{unknown}"
    );

    let base = upstream.base();
    upstream.route(
        "/api/v1/model-versions/67890",
        format!(
            r#"{{"id": 67890, "modelId": 12345, "files": [
                {{"id": 111, "name": "a.bin", "downloadUrl": "{base}/a"}},
                {{"id": 222, "name": "b.bin", "downloadUrl": "{base}/b"}}
            ]}}"#
        )
        .as_bytes(),
    );
    let ambiguous =
        pull_civitai(&mut mirror, &upstream, Some(67890), &Selection::Primary).unwrap_err();
    assert!(
        ambiguous.to_string().contains("no single primary file"),
        "{ambiguous}"
    );
    assert_eq!(upstream.downloads(), Vec::<String>::new());
}

#[test]
fn civitai_pull_rejects_a_version_of_another_model() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    site_with_model(&upstream);
    upstream.route(
        "/api/v1/model-versions/67890",
        br#"{"id": 67890, "modelId": 999, "files": []}"#,
    );
    let mut mirror = Mirror::create(dir.path()).unwrap().0;

    let error = pull_civitai(&mut mirror, &upstream, Some(67890), &Selection::Primary).unwrap_err();

    assert!(
        error.to_string().contains("belongs to model 999"),
        "{error}"
    );
}

#[test]
fn civitai_pull_goes_on_without_the_description_of_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    site_with_model(&upstream);
    upstream
        .routes
        .lock()
        .unwrap()
        .remove("/api/v1/models/12345");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    let site = Site::new(&upstream.base(), None);

    let mut warnings = Vec::new();
    let pulled = mirror
        .pull_civitai(
            &http(),
            &CivitaiPull {
                site: &site,
                host: SITE,
                model_id: 12345,
                version_id: Some(67890),
                selection: &Selection::Primary,
            },
            &mut |event| {
                if let Event::Warning(message) = event {
                    warnings.push(message.to_owned());
                }
            },
        )
        .unwrap();

    assert_eq!(pulled.downloaded, 1);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("model 12345"), "{}", warnings[0]);
}

#[test]
fn civitai_pull_does_not_send_the_token_to_another_host() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    let cdn = Upstream::start();
    cdn.route("/file", b"weights");
    upstream.route(
        "/api/v1/model-versions/67890",
        format!(
            r#"{{"id": 67890, "modelId": 12345, "files": [
                {{"id": 111, "name": "model.safetensors", "downloadUrl": "{}/file"}}
            ]}}"#,
            cdn.base()
        )
        .as_bytes(),
    );
    upstream.route("/api/v1/models/12345", br#"{"id": 12345}"#);
    let mut mirror = Mirror::create(dir.path()).unwrap().0;

    // The download URL of a test server is not https, which the site client
    // refuses for another host; go through the mirror's own fetch instead.
    let result = pull_civitai(&mut mirror, &upstream, Some(67890), &Selection::Primary);

    assert!(matches!(result, Err(Error::Upstream { .. })));
    assert_eq!(cdn.server.requests().len(), 0);
}

#[test]
fn the_same_content_from_two_sources_is_stored_once() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    site_with_model(&upstream);
    let mut mirror = Mirror::create(dir.path()).unwrap().0;

    pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap();
    let pulled = pull_civitai(&mut mirror, &upstream, None, &Selection::Primary).unwrap();

    assert_eq!((pulled.downloaded, pulled.reused), (0, 1));
    assert_eq!(upstream.downloads().len(), 2);
}

#[test]
fn gc_removes_blobs_no_snapshot_uses() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap();
    // A blob that was stored without any snapshot coming to use it.
    let orphan = Sha256::of(b"orphan");
    let partial = mirror.store.partial_path("orphan").unwrap();
    fs::write(&partial, b"orphan").unwrap();
    let key = FileEntry {
        path: "x".into(),
        file_id: None,
        size: None,
        sha256: None,
        primary: false,
        fetched: false,
    };
    let scratch = mirror
        .db
        .record_snapshot("hf:example.com:scratch", "r", std::slice::from_ref(&key))
        .unwrap();
    let store = mirror.store.clone();
    mirror
        .db
        .record_blob(scratch, key.key(), &orphan, 6, || {
            store
                .commit(&partial, &orphan)
                .map(|_| ())
                .map_err(Error::Store)
        })
        .unwrap();
    // The snapshot moves on to other content, leaving the blob unused.
    mirror
        .db
        .record_blob(scratch, key.key(), &Sha256::of(b"{}"), 2, || Ok(()))
        .unwrap();

    // Too young to be removed yet.
    assert_eq!(
        mirror.gc(Duration::from_secs(3600), false).unwrap(),
        Collected::default()
    );

    let would = mirror.gc(Duration::ZERO, true).unwrap();
    assert_eq!(would.blobs, [(orphan, 6)]);
    assert!(mirror.store.contains(&orphan));

    let collected = mirror.gc(Duration::ZERO, false).unwrap();
    assert_eq!(collected.blobs, [(orphan, 6)]);
    assert!(!mirror.store.contains(&orphan));
    assert_eq!(blob(&mirror, b"weights").unwrap(), b"weights");
    assert_eq!(blob(&mirror, b"{}").unwrap(), b"{}");
    assert_eq!(
        mirror.gc(Duration::ZERO, false).unwrap(),
        Collected::default()
    );
    // The lock files of the pull are gone as well.
    assert_eq!(
        fs::read_dir(dir.path().join("private/tmp"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn gc_reports_what_database_and_directory_disagree_on() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    let pulled = pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap();
    let weights = Sha256::of(b"weights");
    mirror.store.remove(&weights).unwrap();
    let stray = Sha256::of(b"stray");
    let stray_path = mirror.store.blob_path(&stray);
    fs::create_dir_all(stray_path.parent().unwrap()).unwrap();
    fs::write(&stray_path, b"stray").unwrap();

    let missing = [MissingBlob {
        sha256: weights,
        snapshots: vec![format!("{}@{COMMIT}", pulled.source)],
    }];
    let would = mirror.gc(Duration::ZERO, true).unwrap();
    assert_eq!(would.missing, missing);

    let collected = mirror.gc(Duration::ZERO, false).unwrap();

    assert_eq!(collected.blobs, []);
    assert_eq!(collected.missing, missing);
    assert!(stray_path.exists());
    assert_eq!(collected.untracked, std::slice::from_ref(&stray_path));
    // Nodes see at once that the file is no longer fetched.
    let file = &hf_snapshot(&mirror, COMMIT)["files"][2];
    assert_eq!(file["path"], "vae/model.safetensors");
    assert_eq!(file["fetched"], false);

    // The blob is forgotten, so there is nothing left to report, and the
    // next pull downloads the file again.
    let again = mirror.gc(Duration::ZERO, false).unwrap();
    assert_eq!(again.missing, []);
    assert_eq!(again.untracked, [stray_path]);
    let pulled = pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap();
    assert_eq!(pulled.downloaded, 1);
    assert_eq!(blob(&mirror, b"weights").unwrap(), b"weights");
    assert_eq!(mirror.gc(Duration::ZERO, false).unwrap().missing, []);
}

#[test]
fn a_mirror_is_opened_only_where_one_was_made() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("mirror");

    let error = Mirror::open(&root).unwrap_err().to_string();
    assert!(error.contains("mvault mirror init"), "{error}");
    assert!(!root.exists());

    let (_, created) = Mirror::create(&root).unwrap();
    assert!(created);
    assert!(root.join("private/mvault.db").is_file());
    let (_, created) = Mirror::create(&root).unwrap();
    assert!(!created);
    Mirror::open(&root).unwrap();
}

#[test]
fn hf_pull_goes_on_after_a_file_that_fails() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    upstream
        .routes
        .lock()
        .unwrap()
        .remove(&format!("/{REPO}/resolve/{COMMIT}/assets/sample.mp4"));
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    let hub = Hub::new(&upstream.base(), None);
    let excludes = Excludes::default();
    let mut log = Vec::new();

    let result = mirror.pull_hf(
        &http(),
        &HfPull {
            hub: &hub,
            host: HOST,
            repo: REPO,
            revision: "main",
            excludes: &excludes,
        },
        &mut |event| match event {
            Event::Started { path, .. } => log.push(path.to_owned()),
            Event::Finished(outcome) => log.push(format!("{outcome:?}")),
            Event::Failed(_) => log.push("failed".to_owned()),
            Event::Progress(_) | Event::Warning(_) => {}
        },
    );

    let error = result.unwrap_err().to_string();
    assert!(
        error.starts_with("1 of 3 file(s) could not be fetched:\n  assets/sample.mp4: "),
        "{error}"
    );
    assert_eq!(
        log,
        [
            "assets/sample.mp4",
            "failed",
            "config.json",
            "Downloaded",
            "vae/model.safetensors",
            "Downloaded"
        ]
    );
    assert_eq!(blob(&mirror, b"weights").unwrap(), b"weights");
    let files = &hf_snapshot(&mirror, COMMIT)["files"];
    assert_eq!(files[0]["fetched"], false);
    assert_eq!(files[2].get("fetched"), None);
    // The name does not move to a snapshot that lacks a file.
    assert!(
        !mirror
            .public_dir()
            .join(format!("snapshots/hf/{HOST}/{REPO}/refs/main"))
            .exists()
    );
}

#[test]
fn export_writes_the_public_tree_again() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap();
    let snapshot = hf_snapshot(&mirror, COMMIT);
    fs::remove_dir_all(mirror.public_dir().join("snapshots")).unwrap();

    assert_eq!(mirror.export_all().unwrap(), 1);

    assert_eq!(hf_snapshot(&mirror, COMMIT), snapshot);
    assert_eq!(
        fs::read_to_string(
            mirror
                .public_dir()
                .join(format!("snapshots/hf/{HOST}/{REPO}/refs/main"))
        )
        .unwrap(),
        format!("{COMMIT}\n")
    );
}

#[test]
fn gc_reports_a_digest_out_of_its_place_and_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    hub_with_repo(&upstream, COMMIT, "main");
    let mut mirror = Mirror::create(dir.path()).unwrap().0;
    pull_hf(&mut mirror, &upstream, "main", &["assets"]).unwrap();
    let weights = Sha256::of(b"weights").to_string();
    let blob_path = mirror.store.blob_path(&Sha256::of(b"weights"));
    let shards = blob_path.parent().unwrap().parent().unwrap();
    let wrong = shards
        .join(if weights.starts_with("00") {
            "01"
        } else {
            "00"
        })
        .join(&weights);
    fs::create_dir_all(wrong.parent().unwrap()).unwrap();
    fs::copy(&blob_path, &wrong).unwrap();

    let collected = mirror.gc(Duration::ZERO, false).unwrap();

    assert_eq!(collected.untracked, std::slice::from_ref(&wrong));
    assert!(wrong.exists());
    assert!(blob_path.exists());
}
