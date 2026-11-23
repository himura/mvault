#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::*;
use crate::download::RetryPolicy;
use crate::hash::Sha256;
use crate::testutil::{Request, Response, TestServer};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// One server standing in for every upstream host.
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
        let upstream = Self { server, routes };
        let base = upstream.server.url("");
        let weights = Sha256::of(b"weights");
        upstream.route(
            "/api/models/example-org/example-model/revision/main?blobs=true",
            format!(
                r#"{{"sha": "{COMMIT}", "siblings": [
                    {{"rfilename": "config.json", "size": 2}},
                    {{"rfilename": "assets/sample.mp4", "size": 6}}
                ]}}"#
            )
            .as_bytes(),
        );
        upstream.route(
            &format!("/example-org/example-model/resolve/{COMMIT}/config.json"),
            b"{}",
        );
        upstream.route(
            &format!("/example-org/example-model/resolve/{COMMIT}/assets/sample.mp4"),
            b"sample",
        );
        upstream.route(
            "/api/v1/models/12345",
            br#"{"id": 12345, "modelVersions": [{"id": 67890}]}"#,
        );
        upstream.route(
            "/api/v1/model-versions/67890",
            format!(
                r#"{{"id": 67890, "modelId": 12345, "files": [
                    {{"id": 111, "name": "model.safetensors", "primary": true,
                      "hashes": {{"SHA256": "{weights}"}}, "downloadUrl": "{base}/download/111"}},
                    {{"id": 222, "name": "extra.zip", "downloadUrl": "{base}/download/222"}}
                ]}}"#
            )
            .as_bytes(),
        );
        upstream.route("/download/111", b"weights");
        upstream.route("/download/222", b"extra");
        upstream
    }

    fn route(&self, path: &str, body: &[u8]) {
        self.routes
            .lock()
            .unwrap()
            .insert(path.to_owned(), body.to_vec());
    }

    fn requests(&self) -> Vec<Request> {
        self.server.requests()
    }
}

struct Run {
    result: Result<()>,
    out: String,
    err: String,
    /// The hosts base URLs were asked for.
    hosts: Vec<String>,
}

fn run(upstream: &Upstream, root: &Path, env: &[(&str, &str)], args: &[&str]) -> Run {
    let mut command = vec!["mirror", "--root", root.to_str().unwrap()];
    command.extend(args);
    run_in(upstream, &std::env::temp_dir(), env, &command)
}

/// Runs `mvault <args>` in the directory `cwd`.
fn run_in(upstream: &Upstream, cwd: &Path, env: &[(&str, &str)], args: &[&str]) -> Run {
    let mut command = vec!["mvault"];
    command.extend(args);
    let cli = Cli::try_parse_from(command).unwrap();
    let env: HashMap<String, String> = env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    let hosts = Mutex::new(Vec::new());
    let base = upstream.server.url("");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let result = execute(
        cli,
        &mut Context {
            env: &|name| env.get(name).cloned(),
            base_url: &|host| {
                hosts.lock().unwrap().push(host.to_owned());
                base.clone()
            },
            http: Client::new(RetryPolicy {
                max_failures: 1,
                ..RetryPolicy::default()
            }),
            cwd: cwd.to_owned(),
            out: &mut out,
            err: &mut err,
            live: false,
        },
    );
    Run {
        result,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
        hosts: hosts.into_inner().unwrap(),
    }
}

fn configure(root: &Path, content: &str) {
    fs::create_dir_all(root.join("private")).unwrap();
    fs::write(root.join("private/config.toml"), content).unwrap();
}

fn has_blob(root: &Path, content: &[u8]) -> bool {
    let digest = Sha256::of(content).to_string();
    root.join("public/blobs/sha256")
        .join(&digest[..2])
        .join(&digest)
        .is_file()
}

fn authorizations(upstream: &Upstream) -> Vec<Option<String>> {
    upstream
        .requests()
        .iter()
        .map(|request| request.header("authorization").map(str::to_owned))
        .collect()
}

#[test]
fn pulls_a_repository_given_as_two_words() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();

    let run = run(
        &upstream,
        dir.path(),
        &[],
        &[
            "pull",
            "hf",
            "example-org/example-model",
            "--exclude",
            "assets",
        ],
    );

    run.result.unwrap();
    assert_eq!(
        run.out,
        format!(
            "hf:huggingface.co:example-org/example-model@{COMMIT}: 1 downloaded, 0 reused, 0 already present, 1 not fetched\n"
        )
    );
    assert_eq!(run.err, "[1/1] config.json  downloaded\n");
    assert_eq!(run.hosts, ["huggingface.co"]);
    assert!(has_blob(dir.path(), b"{}"));
    assert!(!has_blob(dir.path(), b"sample"));
}

#[test]
fn pulls_a_repository_given_as_a_url_with_a_revision() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    upstream.route(
        "/api/models/example-org/example-model/revision/dev?blobs=true",
        format!(r#"{{"sha": "{COMMIT}", "siblings": []}}"#).as_bytes(),
    );

    let run = run(
        &upstream,
        dir.path(),
        &[],
        &[
            "pull",
            "https://hub.example.com/example-org/example-model/tree/dev",
        ],
    );

    run.result.unwrap();
    assert!(
        run.out
            .starts_with("hf:hub.example.com:example-org/example-model@"),
        "{}",
        run.out
    );
    assert_eq!(run.hosts, ["hub.example.com"]);
}

#[test]
fn the_hub_token_goes_to_the_default_hub_only() {
    let upstream = Upstream::start();
    let env = [("HF_TOKEN", "secret")];

    let dir = tempfile::tempdir().unwrap();
    run(
        &upstream,
        dir.path(),
        &env,
        &["pull", "hf:example-org/example-model"],
    )
    .result
    .unwrap();
    let default_hub = authorizations(&upstream);
    assert_eq!(default_hub.len(), 3);
    assert!(
        default_hub
            .iter()
            .all(|value| value.as_deref() == Some("Bearer secret"))
    );

    let dir = tempfile::tempdir().unwrap();
    run(
        &upstream,
        dir.path(),
        &env,
        &[
            "pull",
            "hf",
            "example-org/example-model",
            "--host",
            "hub.example.com",
        ],
    )
    .result
    .unwrap();
    assert!(authorizations(&upstream)[3..].iter().all(Option::is_none));
}

#[test]
fn pulls_the_latest_version_of_a_model_from_the_configured_site() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    configure(dir.path(), "[civitai]\ndefault_host = \"example.com\"\n");

    let run = run(
        &upstream,
        dir.path(),
        &[("CIVITAI_TOKEN", "secret")],
        &["pull", "civitai", "12345"],
    );

    run.result.unwrap();
    assert_eq!(
        run.out,
        "civitai:example.com:12345@67890: 1 downloaded, 0 reused, 0 already present, 1 not fetched\n"
    );
    assert_eq!(run.hosts, ["example.com"]);
    assert!(has_blob(dir.path(), b"weights"));
    assert!(
        authorizations(&upstream)
            .iter()
            .all(|value| value.as_deref() == Some("Bearer secret"))
    );
}

#[test]
fn the_site_token_goes_to_the_configured_site_only() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    configure(dir.path(), "[civitai]\ndefault_host = \"example.com\"\n");

    let run = run(
        &upstream,
        dir.path(),
        &[("CIVITAI_TOKEN", "secret")],
        &[
            "pull",
            "https://other.example.net/models/12345?modelVersionId=67890",
        ],
    );

    run.result.unwrap();
    assert!(
        run.out.starts_with("civitai:other.example.net:12345@67890"),
        "{}",
        run.out
    );
    assert!(authorizations(&upstream).iter().all(Option::is_none));
}

#[test]
fn selects_files_of_a_version_by_flag_and_by_url() {
    let upstream = Upstream::start();

    let dir = tempfile::tempdir().unwrap();
    let by_flag = run(
        &upstream,
        dir.path(),
        &[],
        &[
            "pull",
            "civitai",
            "12345@67890",
            "--host",
            "example.com",
            "--file-id",
            "222",
        ],
    );
    by_flag.result.unwrap();
    assert!(has_blob(dir.path(), b"extra"));
    assert!(!has_blob(dir.path(), b"weights"));

    let dir = tempfile::tempdir().unwrap();
    let by_url = run(
        &upstream,
        dir.path(),
        &[],
        &[
            "pull",
            "https://example.com/models/12345?modelVersionId=67890&fileId=222",
        ],
    );
    by_url.result.unwrap();
    assert!(has_blob(dir.path(), b"extra"));
    assert!(!has_blob(dir.path(), b"weights"));

    let dir = tempfile::tempdir().unwrap();
    let all = run(
        &upstream,
        dir.path(),
        &[],
        &["pull", "civitai:example.com:12345@67890", "--all-files"],
    );
    all.result.unwrap();
    assert!(has_blob(dir.path(), b"extra"));
    assert!(has_blob(dir.path(), b"weights"));
}

#[test]
fn rejects_arguments_that_do_not_fit_the_source() {
    let upstream = Upstream::start();
    for (args, expected) in [
        (vec!["pull", "civitai", "12345"], "no default civitai host"),
        (
            vec!["pull", "hf:example-org/example-model", "--file-id", "1"],
            "--file-id and --all-files are for model versions",
        ),
        (
            vec!["pull", "hf:example-org/example-model", "--all-files"],
            "--file-id and --all-files are for model versions",
        ),
        (
            vec!["pull", "civitai:example.com:12345", "--exclude", "x"],
            "--revision and --exclude are for repositories",
        ),
        (
            vec!["pull", "civitai:example.com:12345", "--revision", "main"],
            "--revision and --exclude are for repositories",
        ),
        (
            vec![
                "pull",
                "https://huggingface.co/example-org/example-model/blob/main/config.json",
            ],
            "the URL names a file",
        ),
        (
            vec![
                "pull",
                "https://huggingface.co/example-org/example-model/tree/dev",
                "--revision",
                "main",
            ],
            "contradicts the revision dev",
        ),
        (
            vec![
                "pull",
                "hf:example-org/example-model",
                "--host",
                "example.com",
            ],
            "--host is for the forms",
        ),
        (
            vec!["pull", "other", "example-org/example-model"],
            "expected `hf <repo>` or `civitai",
        ),
        (
            vec!["pull", "http://huggingface.co/example-org/example-model"],
            "invalid source",
        ),
        (
            vec!["pull", "hf:example-org/example-model", "--exclude", "/(/"],
            "invalid exclude pattern",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let run = run(&upstream, dir.path(), &[], &args);
        let error = run.result.unwrap_err().to_string();
        assert!(error.contains(expected), "{args:?}: {error}");
    }
    assert_eq!(upstream.requests().len(), 0);
}

#[test]
fn command_line_mistakes_are_caught_by_the_parser() {
    for args in [
        vec!["mvault"],
        vec!["mvault", "mirror"],
        vec!["mvault", "mirror", "pull"],
        vec!["mvault", "mirror", "pull", "a", "b", "c"],
        vec!["mvault", "mirror", "pull", "x", "--file-id", "abc"],
        vec![
            "mvault",
            "mirror",
            "pull",
            "x",
            "--file-id",
            "1",
            "--all-files",
        ],
    ] {
        assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
    }
}

#[test]
fn the_mirror_root_is_required() {
    let upstream = Upstream::start();
    let cli =
        Cli::try_parse_from(["mvault", "mirror", "pull", "hf:example-org/example-model"]).unwrap();
    // The environment of the test process may name a mirror.
    if std::env::var_os("MVAULT_MIRROR_ROOT").is_some() {
        return;
    }
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let result = execute(
        cli,
        &mut Context {
            env: &|_| None,
            base_url: &|_| upstream.server.url(""),
            http: Client::default(),
            cwd: std::env::temp_dir(),
            out: &mut out,
            err: &mut err,
            live: false,
        },
    );
    assert!(result.unwrap_err().to_string().contains("--root"));
}

#[test]
fn a_live_reporter_redraws_the_line() {
    let mut err = Vec::new();
    let mut reporter = Reporter {
        err: &mut err,
        live: true,
        label: String::new(),
        drawn: None,
    };
    reporter.handle(Event::Started {
        index: 1,
        count: 2,
        path: "model.safetensors",
    });
    reporter.handle(Event::Progress(crate::download::Progress {
        done: 1536,
        total: Some(4096),
    }));
    // Too soon after the last redraw to be shown.
    reporter.handle(Event::Progress(crate::download::Progress {
        done: 2048,
        total: Some(4096),
    }));
    reporter.handle(Event::Finished(Outcome::Downloaded));
    reporter.handle(Event::Warning("careful"));

    assert_eq!(
        String::from_utf8(err).unwrap(),
        "[1/2] model.safetensors\
         \r\x1b[K[1/2] model.safetensors  1.5 KiB / 4.0 KiB\
         \r\x1b[K[1/2] model.safetensors  downloaded\n\
         warning: careful\n"
    );
}

#[test]
fn sizes_are_shown_with_binary_units() {
    for (bytes, text) in [
        (0, "0 B"),
        (1023, "1023 B"),
        (1024, "1.0 KiB"),
        (1536, "1.5 KiB"),
        (1024 * 1024 - 1, "1023.9 KiB"),
        (5 * 1024 * 1024, "5.0 MiB"),
        (3 * 1024 * 1024 * 1024 + 512 * 1024 * 1024, "3.5 GiB"),
        (2 * 1024_u64.pow(4), "2.0 TiB"),
        (u64::MAX, "16777215.9 TiB"),
    ] {
        assert_eq!(human_size(bytes), text);
    }
}

/// A mirror that pulled the repository of the test upstream without
/// `assets/`, and an initialized workspace that uses it.
fn mirror_and_workspace(upstream: &Upstream, dir: &Path) -> std::path::PathBuf {
    let mirror = dir.join("mirror");
    run(
        upstream,
        &mirror,
        &[],
        &[
            "pull",
            "hf",
            "example-org/example-model",
            "--exclude",
            "assets",
        ],
    )
    .result
    .unwrap();
    let workspace = dir.join("mlenv");
    let public = mirror.join("public");
    let init = run_in(
        upstream,
        dir,
        &[],
        &["init", "mlenv", "--mirror", public.to_str().unwrap()],
    );
    init.result.unwrap();
    assert_eq!(init.out.lines().count(), 3, "{}", init.out);
    workspace
}

#[test]
fn a_model_goes_from_the_upstream_through_the_mirror_into_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    let workspace = mirror_and_workspace(&upstream, dir.path());
    let requests = upstream.requests().len();

    let added = run_in(
        &upstream,
        &workspace,
        &[],
        &[
            "add",
            "https://huggingface.co/example-org/example-model/blob/main/config.json",
            "--dest",
            "models/x/",
        ],
    );
    added.result.unwrap();
    assert_eq!(
        added.out,
        format!(
            "added to mvault.toml (resolved to {COMMIT}):\n\n[[model]]\nsource = \"hf:example-org/example-model\"\n\
             files = [\n  {{ path = \"config.json\", dest = \"models/x/\" }},\n]\n\n"
        )
    );
    assert!(!workspace.join("models").exists());

    // From a directory below the workspace, as one would while working there.
    fs::create_dir_all(workspace.join("models")).unwrap();
    let synced = run_in(&upstream, &workspace.join("models"), &[], &["sync"]);
    synced.result.unwrap();
    assert_eq!(
        synced.out,
        "0 resolved, 1 fetched, 1 linked, 0 unchanged, 0 pruned (done)\n"
    );
    assert_eq!(synced.err, "[1/1] models/x/config.json  fetched\n");
    assert_eq!(
        fs::read(workspace.join("models/x/config.json")).unwrap(),
        b"{}"
    );

    let again = run_in(&upstream, &workspace, &[], &["sync", "-n"]);
    again.result.unwrap();
    assert_eq!(
        again.out,
        "0 resolved, 0 fetched, 0 linked, 1 unchanged, 0 pruned (would be done)\n"
    );

    // The mirror answered everything the node asked.
    assert_eq!(upstream.requests().len(), requests);
}

#[test]
fn sync_reports_conflicts_and_a_missing_file_as_errors() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    let workspace = mirror_and_workspace(&upstream, dir.path());
    let add = |dest: &str, path: &str| {
        run_in(
            &upstream,
            &workspace,
            &[],
            &[
                "add",
                "hf:example-org/example-model",
                "--dest",
                dest,
                "--path",
                path,
            ],
        )
    };

    add("models/x/", "config.json").result.unwrap();
    fs::create_dir_all(workspace.join("models/x")).unwrap();
    fs::write(workspace.join("models/x/config.json"), b"mine").unwrap();
    let conflict = run_in(&upstream, &workspace, &[], &["sync"]);
    assert!(
        conflict
            .result
            .unwrap_err()
            .to_string()
            .contains("1 destination(s) are taken"),
    );
    assert_eq!(
        conflict.err,
        "[1/1] models/x/config.json  fetched\n\
         conflict: models/x/config.json exists and is not managed by mvault (--force replaces it)\n"
    );
    run_in(&upstream, &workspace, &[], &["sync", "--force"])
        .result
        .unwrap();
    assert_eq!(
        fs::read(workspace.join("models/x/config.json")).unwrap(),
        b"{}"
    );

    add("models/assets/", "assets/*").result.unwrap();
    let missing = run_in(&upstream, &workspace, &[], &["sync"]);
    let error = missing.result.unwrap_err().to_string();
    assert!(
        error.contains(
            "on the mirror host run: mvault mirror pull hf:huggingface.co:example-org/example-model"
        ),
        "{error}"
    );
}

#[test]
fn without_a_mirror_the_upstream_is_used_with_the_token_of_its_host() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    let home = dir.path().join("home");
    fs::create_dir_all(home.join(".config/mvault")).unwrap();
    fs::write(
        home.join(".config/mvault/config.toml"),
        "allow_official = true\n[civitai]\ndefault_host = \"example.com\"\n",
    )
    .unwrap();
    let env = [
        ("HOME", home.to_str().unwrap()),
        ("HF_TOKEN", "hub-secret"),
        ("CIVITAI_TOKEN", "site-secret"),
    ];
    let workspace = dir.path().join("mlenv");
    run_in(&upstream, dir.path(), &env, &["init", "mlenv"])
        .result
        .unwrap();

    for (source, dest) in [
        ("hf:example-org/example-model", "models/x/"),
        ("hf:hub.example.com:example-org/example-model", "models/y/"),
        ("civitai:12345", "models/loras/"),
        ("https://other.example.net/models/12345", "models/other/"),
    ] {
        run_in(
            &upstream,
            &workspace,
            &env,
            &["add", source, "--dest", dest],
        )
        .result
        .unwrap();
    }
    let synced = run_in(&upstream, &workspace, &env, &["sync"]);
    synced.result.unwrap();
    assert_eq!(
        synced.out,
        "0 resolved, 5 fetched, 6 linked, 0 unchanged, 0 pruned (done)\n"
    );
    assert_eq!(
        fs::read(workspace.join("models/loras/model.safetensors")).unwrap(),
        b"weights"
    );
    assert_eq!(
        fs::read(workspace.join("models/y/assets/sample.mp4")).unwrap(),
        b"sample"
    );

    // Each token went to its own host and nowhere else. The test server
    // stands for all hosts, so the requests are told apart by their order:
    // one resolution per add, then the downloads.
    let sent = authorizations(&upstream);
    let hosts = synced.hosts;
    assert_eq!(sent[0].as_deref(), Some("Bearer hub-secret"));
    assert_eq!(sent[1], None);
    assert_eq!(sent[2].as_deref(), Some("Bearer site-secret"));
    assert_eq!(sent[3].as_deref(), Some("Bearer site-secret"));
    assert_eq!(sent[4], None);
    assert_eq!(sent[5], None);
    assert!(hosts.contains(&"huggingface.co".to_owned()));
    assert!(hosts.contains(&"example.com".to_owned()));
}

#[test]
fn node_commands_need_a_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    for args in [
        vec!["sync"],
        vec!["add", "hf:example-org/example-model", "--dest", "x/"],
    ] {
        let run = run_in(&upstream, dir.path(), &[], &args);
        let error = run.result.unwrap_err().to_string();
        assert!(error.contains("mvault init creates one"), "{error}");
    }

    let first = run_in(&upstream, dir.path(), &[], &["init"]);
    first.result.unwrap();
    assert!(first.out.contains("created"), "{}", first.out);
    let second = run_in(&upstream, dir.path(), &[], &["init"]);
    second.result.unwrap();
    assert!(
        second.out.contains("is already a workspace"),
        "{}",
        second.out
    );
}

#[test]
fn mirror_gc_keeps_what_snapshots_use() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    run(
        &upstream,
        dir.path(),
        &[],
        &["pull", "hf:example-org/example-model"],
    )
    .result
    .unwrap();

    let young = run(&upstream, dir.path(), &[], &["gc"]);
    young.result.unwrap();
    assert_eq!(
        young.out,
        "removed 0 blob(s) (0 B), 0 unfinished download(s)\n"
    );
    let all = run(&upstream, dir.path(), &[], &["gc", "-n", "--min-age", "0"]);
    all.result.unwrap();
    assert_eq!(
        all.out,
        "would remove 0 blob(s) (0 B), 0 unfinished download(s)\n"
    );
    assert!(has_blob(dir.path(), b"sample"));

    assert!(Cli::try_parse_from(["mvault", "mirror", "gc", "--min-age", "soon"]).is_err());
}

#[cfg(unix)]
#[test]
fn gc_removes_blobs_of_files_that_were_pruned() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = Upstream::start();
    let workspace = mirror_and_workspace(&upstream, dir.path());
    run_in(
        &upstream,
        &workspace,
        &[],
        &[
            "add",
            "hf:example-org/example-model",
            "--dest",
            "x/",
            "--path",
            "config.json",
        ],
    )
    .result
    .unwrap();
    run_in(&upstream, &workspace, &[], &["sync"])
        .result
        .unwrap();

    let nothing = run_in(&upstream, &workspace, &[], &["gc"]);
    nothing.result.unwrap();
    assert_eq!(
        nothing.out,
        "removed 0 blob(s) (0 B), 0 unfinished download(s)\n"
    );

    fs::remove_file(workspace.join("x/config.json")).unwrap();
    let would = run_in(&upstream, &workspace, &[], &["gc", "--dry-run"]);
    assert_eq!(
        would.out,
        "would remove 1 blob(s) (2 B), 0 unfinished download(s)\n"
    );
    let removed = run_in(&upstream, &workspace, &[], &["gc"]);
    assert_eq!(
        removed.out,
        "removed 1 blob(s) (2 B), 0 unfinished download(s)\n"
    );

    // The next sync fetches it again.
    let synced = run_in(&upstream, &workspace, &[], &["sync"]);
    assert_eq!(
        synced.out,
        "0 resolved, 1 fetched, 1 linked, 0 unchanged, 0 pruned (done)\n"
    );
}
