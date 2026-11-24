#![allow(clippy::unwrap_used)]

use std::cell::RefCell;

use super::*;
use crate::download::{Client, RetryPolicy};
use crate::mirror::exclude::Excludes;
use crate::mirror::{CivitaiPull, HfPull, Mirror, Selection};
use crate::node::manifest::{LOCK_NAME, MANIFEST_NAME};
use crate::node::placed::{Placed, Stamp};
use crate::node::remote::Remote;
use crate::testutil::{Routes, serve_dir};
use crate::upstream::civitai::Site;
use crate::upstream::hf::Hub;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const REPO: &str = "example-org/example-model";
const HUB: &str = "hub.example.com";
const SITE: &str = "example.com";

const REPO_ENTRY: &str = r#"
[[model]]
source = "hf:hub.example.com:example-org/example-model"
files = [
  { path = "config.json", dest = "models/x/" },
  { path = "vae/model.safetensors", dest = "models/vae/x_vae.safetensors" },
]
"#;

fn http() -> Client {
    Client::new(RetryPolicy {
        max_failures: 1,
        ..RetryPolicy::default()
    })
}

/// An upstream with a repository and a model, a mirror that pulled the
/// repository without `assets/` and the primary file of the model, and an
/// empty workspace.
struct World {
    dir: tempfile::TempDir,
    upstream: Routes,
}

impl World {
    fn new() -> Self {
        let upstream = Routes::start();
        let base = upstream.base();
        let weights = Sha256::of(b"weights");
        let sample = Sha256::of(b"sample");
        upstream.route(
            &format!("/api/models/{REPO}/revision/main?blobs=true"),
            format!(
                r#"{{"sha": "{COMMIT}", "siblings": [
                    {{"rfilename": "config.json", "size": 2}},
                    {{"rfilename": "vae/model.safetensors", "size": 7, "lfs": {{"sha256": "{weights}", "size": 7}}}},
                    {{"rfilename": "assets/sample.mp4", "size": 6, "lfs": {{"sha256": "{sample}", "size": 6}}}}
                ]}}"#
            )
            .as_bytes(),
        );
        upstream.route(&format!("/{REPO}/resolve/{COMMIT}/config.json"), b"{}");
        upstream.route(
            &format!("/{REPO}/resolve/{COMMIT}/vae/model.safetensors"),
            b"weights",
        );
        upstream.route(
            &format!("/{REPO}/resolve/{COMMIT}/assets/sample.mp4"),
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
                    {{"id": 111, "name": "lora.safetensors", "primary": true,
                      "hashes": {{"SHA256": "{}"}}, "downloadUrl": "{base}/download/111"}},
                    {{"id": 222, "name": "extra.zip", "downloadUrl": "{base}/download/222"}}
                ]}}"#,
                Sha256::of(b"lora")
            )
            .as_bytes(),
        );
        upstream.route("/download/111", b"lora");
        upstream.route("/download/222", b"extra");

        let world = Self {
            dir: tempfile::tempdir().unwrap(),
            upstream,
        };
        let mut mirror = Mirror::open(&world.mirror_root()).unwrap();
        let hub = Hub::new(&base, None);
        let site = Site::new(&base, None);
        mirror
            .pull_hf(
                &http(),
                &HfPull {
                    hub: &hub,
                    host: HUB,
                    repo: REPO,
                    revision: "main",
                    excludes: &Excludes::new(&["assets"]).unwrap(),
                },
                &mut |_| {},
            )
            .unwrap();
        mirror
            .pull_civitai(
                &http(),
                &CivitaiPull {
                    site: &site,
                    host: SITE,
                    model_id: 12345,
                    version_id: None,
                    selection: &Selection::Primary,
                },
                &mut |_| {},
            )
            .unwrap();
        fs::create_dir_all(world.root()).unwrap();
        world
    }

    fn mirror_root(&self) -> PathBuf {
        self.dir.path().join("mirror")
    }

    fn public(&self) -> PathBuf {
        self.mirror_root().join("public")
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("workspace")
    }

    fn manifest(&self, text: &str) {
        fs::write(self.root().join(MANIFEST_NAME), text).unwrap();
    }

    fn settings(&self, text: &str) {
        fs::create_dir_all(self.root().join(".mvault")).unwrap();
        fs::write(self.root().join(".mvault/config.toml"), text).unwrap();
    }

    fn lock_text(&self) -> String {
        fs::read_to_string(self.root().join(LOCK_NAME)).unwrap_or_default()
    }

    fn read(&self, path: &str) -> Vec<u8> {
        fs::read(self.root().join(path)).unwrap()
    }

    fn exists(&self, path: &str) -> bool {
        self.root().join(path).exists()
    }

    fn upstream_requests(&self) -> usize {
        self.upstream.server.requests().len()
    }

    /// Syncs with the given mirrors, returning the result, the events and
    /// the warnings.
    fn sync_with(
        &self,
        remotes: &[Remote],
        options: Options,
    ) -> (Result<Report>, Vec<String>, Vec<String>) {
        let workspace = Workspace::open(self.root(), None).unwrap();
        let base = self.upstream.base();
        let http = http();
        let access = Access {
            http: &http,
            remotes,
            hub: &|_| Hub::new(&base, None),
            site: &|_| Site::new(&base, None),
            warnings: RefCell::default(),
        };
        let mut events = Vec::new();
        let result = sync(&workspace, &access, options, &mut |event| match event {
            Event::Progress(_) => {}
            other => events.push(format!("{other:?}")),
        });
        (result, events, access.warnings.into_inner())
    }

    fn sync(&self, options: Options) -> Result<Report> {
        self.sync_with(&[Remote::Dir(self.public())], options).0
    }
}

fn is_link_to_blob(world: &World, path: &str, content: &[u8]) -> bool {
    let workspace = Workspace::open(world.root(), None).unwrap();
    let blob = workspace.store.blob_path(&Sha256::of(content));
    same_file::is_same_file(world.root().join(path), blob).unwrap_or(false)
}

#[test]
fn sync_places_files_from_a_mounted_mirror() {
    let world = World::new();
    world.manifest(REPO_ENTRY);
    let before = world.upstream_requests();

    let (result, events, warnings) =
        world.sync_with(&[Remote::Dir(world.public())], Options::default());

    assert_eq!(
        result.unwrap(),
        Report {
            resolved: 1,
            fetched: 2,
            linked: 2,
            ..Report::default()
        }
    );
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(world.read("models/x/config.json"), b"{}");
    assert_eq!(world.read("models/vae/x_vae.safetensors"), b"weights");
    assert!(is_link_to_blob(
        &world,
        "models/vae/x_vae.safetensors",
        b"weights"
    ));
    assert!(
        fs::metadata(world.root().join("models/x/config.json"))
            .unwrap()
            .permissions()
            .readonly()
    );
    // The mirror answered everything; the upstream was not asked.
    assert_eq!(world.upstream_requests(), before);
    assert_eq!(
        events,
        [
            format!("Resolved {{ source: \"hf:{HUB}:{REPO}\", resolved: \"{COMMIT}\" }}"),
            "Fetching { index: 1, count: 2, dest: \"models/vae/x_vae.safetensors\" }".to_owned(),
            "Fetched".to_owned(),
            "Fetching { index: 2, count: 2, dest: \"models/x/config.json\" }".to_owned(),
            "Fetched".to_owned(),
            "Linked(\"models/vae/x_vae.safetensors\")".to_owned(),
            "Linked(\"models/x/config.json\")".to_owned(),
        ]
    );

    // The lock lists every file of the commit, with the digests the mirror
    // knows: also for the file the hub announced none for.
    let lock = Lock::load(&world.root().join(LOCK_NAME)).unwrap();
    let locked = lock.get(&format!("hf:{HUB}:{REPO}"), Some("main")).unwrap();
    assert_eq!(locked.resolved, COMMIT);
    let digests: Vec<_> = locked
        .files
        .iter()
        .map(|file| (file.path.as_str(), file.sha256))
        .collect();
    assert_eq!(
        digests,
        [
            ("assets/sample.mp4", Some(Sha256::of(b"sample"))),
            ("config.json", Some(Sha256::of(b"{}"))),
            ("vae/model.safetensors", Some(Sha256::of(b"weights"))),
        ]
    );
}

#[test]
fn a_second_sync_changes_nothing() {
    let world = World::new();
    world.manifest(REPO_ENTRY);
    world.sync(Options::default()).unwrap();
    let lock = world.lock_text();

    let again = world.sync(Options::default()).unwrap();

    assert_eq!(
        again,
        Report {
            unchanged: 2,
            ..Report::default()
        }
    );
    assert_eq!(world.lock_text(), lock);
}

#[test]
fn sync_places_files_from_a_mirror_behind_a_web_server() {
    let world = World::new();
    world.manifest(&format!(
        "{REPO_ENTRY}\n[[model]]\nsource = \"civitai:{SITE}:12345\"\ndest = \"models/loras/\"\n"
    ));
    let server = serve_dir(&world.public());
    let before = world.upstream_requests();

    let (result, _, warnings) = world.sync_with(
        &[Remote::parse(&server.url("/")).unwrap()],
        Options::default(),
    );

    let report = result.unwrap();
    assert_eq!((report.resolved, report.fetched, report.linked), (2, 3, 3));
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(world.read("models/loras/lora.safetensors"), b"lora");
    assert_eq!(world.read("models/vae/x_vae.safetensors"), b"weights");
    assert_eq!(world.upstream_requests(), before);
    // "latest" was resolved through the mirror and pinned.
    let lock = Lock::load(&world.root().join(LOCK_NAME)).unwrap();
    assert_eq!(
        lock.get(&format!("civitai:{SITE}:12345"), None)
            .unwrap()
            .resolved,
        "67890"
    );
}

#[test]
fn a_file_the_mirror_left_out_is_an_error_with_the_way_to_get_it() {
    let world = World::new();
    world.manifest(&format!(
        "[[model]]\nsource = \"hf:{HUB}:{REPO}\"\ndest = \"models/x/\"\n"
    ));

    let error = world.sync(Options::default()).unwrap_err().to_string();

    assert_eq!(
        error,
        format!(
            "models/x/assets/sample.mp4: no mirror has assets/sample.mp4 of hf:{HUB}:{REPO}; \
             on the mirror host run: mvault mirror pull hf:{HUB}:{REPO} --revision {COMMIT}"
        )
    );
    // The model stays resolved, so the next sync starts from the lock.
    assert!(world.lock_text().contains(COMMIT));
}

#[test]
fn the_upstream_fills_in_when_the_settings_allow() {
    let world = World::new();
    world.manifest(&format!(
        "[[model]]\nsource = \"hf:{HUB}:{REPO}\"\ndest = \"models/x/\"\n\
         [[model]]\nsource = \"civitai:{SITE}:12345@67890\"\nfiles = [{{ file_id = 222, dest = \"extra/\" }}]\n"
    ));
    world.settings("allow_official = true\n");

    let report = world.sync(Options::default()).unwrap();

    assert_eq!((report.fetched, report.linked), (4, 4));
    assert_eq!(world.read("models/x/assets/sample.mp4"), b"sample");
    assert_eq!(world.read("extra/extra.zip"), b"extra");
    // The digest of the file nobody announced one for is now in the lock.
    assert!(
        world
            .lock_text()
            .contains(&Sha256::of(b"extra").to_string())
    );
}

#[test]
fn without_a_mirror_everything_comes_from_the_upstream() {
    let world = World::new();
    world.manifest(REPO_ENTRY);
    world.settings("allow_official = true\n");

    let (result, _, _) = world.sync_with(&[], Options::default());

    let report = result.unwrap();
    assert_eq!((report.resolved, report.fetched, report.linked), (1, 2, 2));
    assert_eq!(world.read("models/x/config.json"), b"{}");
    let lock = Lock::load(&world.root().join(LOCK_NAME)).unwrap();
    let locked = lock.get(&format!("hf:{HUB}:{REPO}"), Some("main")).unwrap();
    let config = locked
        .files
        .iter()
        .find(|file| file.path == "config.json")
        .unwrap();
    assert_eq!(config.sha256, Some(Sha256::of(b"{}")));
}

#[test]
fn without_a_mirror_and_without_leave_nothing_is_fetched() {
    let world = World::new();
    world.manifest(REPO_ENTRY);

    let (result, _, _) = world.sync_with(&[], Options::default());

    let error = result.unwrap_err().to_string();
    assert!(error.contains("no mirror has"), "{error}");
    assert!(!world.exists("models"));
}

#[test]
fn locked_refuses_to_resolve_and_dry_run_writes_nothing() {
    let world = World::new();
    world.manifest(REPO_ENTRY);

    let locked = world
        .sync(Options {
            locked: true,
            ..Options::default()
        })
        .unwrap_err();
    assert!(locked.to_string().contains("--locked"), "{locked}");

    let dry = world
        .sync(Options {
            dry_run: true,
            ..Options::default()
        })
        .unwrap();
    assert_eq!(
        dry,
        Report {
            resolved: 1,
            fetched: 2,
            linked: 2,
            ..Report::default()
        }
    );
    assert!(!world.exists(LOCK_NAME));
    assert!(!world.exists("models"));
    assert!(!world.exists(".mvault/blobs"));

    world.sync(Options::default()).unwrap();
    let after = world
        .sync(Options {
            locked: true,
            ..Options::default()
        })
        .unwrap();
    assert_eq!(after.unchanged, 2);
}

#[test]
fn a_file_of_the_user_is_not_replaced_unless_forced() {
    let world = World::new();
    world.manifest(REPO_ENTRY);
    fs::create_dir_all(world.root().join("models/x")).unwrap();
    fs::write(world.root().join("models/x/config.json"), b"mine").unwrap();

    let (result, events, _) = world.sync_with(&[Remote::Dir(world.public())], Options::default());

    let report = result.unwrap();
    assert_eq!(report.conflicts, ["models/x/config.json"]);
    assert_eq!(report.linked, 1);
    assert!(events.contains(&"Conflict(\"models/x/config.json\")".to_owned()));
    assert_eq!(world.read("models/x/config.json"), b"mine");

    let forced = world
        .sync(Options {
            force: true,
            ..Options::default()
        })
        .unwrap();
    assert_eq!((forced.linked, forced.unchanged), (1, 1));
    assert_eq!(forced.conflicts, Vec::<String>::new());
    assert_eq!(world.read("models/x/config.json"), b"{}");
}

#[test]
fn a_directory_in_the_way_is_a_conflict() {
    let world = World::new();
    world.manifest(REPO_ENTRY);
    fs::create_dir_all(world.root().join("models/x/config.json")).unwrap();

    let report = world.sync(Options::default()).unwrap();

    assert_eq!(report.conflicts, ["models/x/config.json"]);
}

#[test]
fn a_managed_file_is_replaced_when_the_manifest_changes() {
    let world = World::new();
    let entry = |path: &str| {
        format!(
            "[[model]]\nsource = \"hf:{HUB}:{REPO}\"\nfiles = [{{ path = \"{path}\", dest = \"models/current.bin\" }}]\n"
        )
    };
    world.manifest(&entry("config.json"));
    world.sync(Options::default()).unwrap();
    assert_eq!(world.read("models/current.bin"), b"{}");
    let lock = world.lock_text();

    world.manifest(&entry("vae/model.safetensors"));
    let report = world.sync(Options::default()).unwrap();

    // No new resolution: the lock already lists every file of the commit.
    assert_eq!((report.resolved, report.fetched, report.linked), (0, 1, 1));
    assert_eq!(report.conflicts, Vec::<String>::new());
    assert_eq!(world.read("models/current.bin"), b"weights");
    assert_eq!(world.lock_text(), lock);
}

#[test]
fn prune_removes_recorded_files_only() {
    let world = World::new();
    world.manifest(REPO_ENTRY);
    world.sync(Options::default()).unwrap();
    fs::write(world.root().join("models/x/notes.txt"), b"mine").unwrap();
    // The same size as a blob, but not a link to one.
    fs::write(world.root().join("models/x/copy.json"), b"{}").unwrap();
    // A link to a blob that mvault did not place: not in the record.
    let workspace = Workspace::open(world.root(), None).unwrap();
    fs::hard_link(
        workspace.store.blob_path(&Sha256::of(b"{}")),
        world.root().join("models/x/linked-by-hand.json"),
    )
    .unwrap();

    world.manifest(&format!(
        "[[model]]\nsource = \"hf:{HUB}:{REPO}\"\nfiles = [{{ path = \"config.json\", dest = \"models/x/renamed.json\" }}]\n"
    ));
    let without = world.sync(Options::default()).unwrap();
    assert_eq!((without.linked, without.pruned), (1, 0));
    assert_eq!(
        without.dropped,
        ["models/vae/x_vae.safetensors", "models/x/config.json"]
    );
    assert!(world.exists("models/x/config.json"));

    let dry = world
        .sync(Options {
            prune: true,
            dry_run: true,
            ..Options::default()
        })
        .unwrap();
    assert_eq!(dry.pruned, 2);
    assert!(world.exists("models/x/config.json"));

    let (result, events, _) = world.sync_with(
        &[Remote::Dir(world.public())],
        Options {
            prune: true,
            ..Options::default()
        },
    );
    assert_eq!(result.unwrap().pruned, 2);
    assert_eq!(
        events,
        [
            "Pruned(\"models/vae/x_vae.safetensors\")",
            "Pruned(\"models/x/config.json\")"
        ]
    );
    assert!(!world.exists("models/x/config.json"));
    assert!(!world.exists("models/vae/x_vae.safetensors"));
    assert_eq!(world.read("models/x/renamed.json"), b"{}");
    assert_eq!(world.read("models/x/notes.txt"), b"mine");
    assert_eq!(world.read("models/x/copy.json"), b"{}");
    assert_eq!(world.read("models/x/linked-by-hand.json"), b"{}");
    // The blobs stay: putting the files back in the manifest fetches nothing.
    assert!(workspace.store.contains(&Sha256::of(b"weights")));
    let record = Placed::load(&workspace.placed_path()).unwrap();
    let recorded: Vec<&str> = record.iter().map(|(dest, _)| dest).collect();
    assert_eq!(recorded, ["models/x/renamed.json"]);
}

#[test]
fn files_placed_before_the_record_existed_are_not_pruned() {
    let world = World::new();
    world.manifest(REPO_ENTRY);
    world.sync(Options::default()).unwrap();
    let workspace = Workspace::open(world.root(), None).unwrap();
    fs::remove_file(workspace.placed_path()).unwrap();

    world.manifest("");
    let report = world
        .sync(Options {
            prune: true,
            ..Options::default()
        })
        .unwrap();

    assert_eq!(report.pruned, 0);
    assert!(world.exists("models/x/config.json"));
}

#[test]
fn every_placed_file_is_recorded_with_its_blob() {
    let world = World::new();
    world.manifest(REPO_ENTRY);

    world.sync(Options::default()).unwrap();

    let workspace = Workspace::open(world.root(), None).unwrap();
    let record = Placed::load(&workspace.placed_path()).unwrap();
    let entry = record.get("models/vae/x_vae.safetensors").unwrap();
    assert_eq!(entry.sha256, Sha256::of(b"weights"));
    let blob = workspace.store.blob_path(&entry.sha256);
    assert!(Stamp::of(&blob).unwrap().unwrap().matches(entry));
    assert_eq!(record.iter().count(), 2);
    // No temporary link is left next to the blobs or in the workspace.
    let tmp: Vec<_> = fs::read_dir(workspace.store.tmp_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with(LINK_PREFIX))
        .collect();
    assert_eq!(tmp, Vec::<String>::new());
    let names: Vec<_> = fs::read_dir(world.root().join("models/vae"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["x_vae.safetensors"]);
}

#[test]
fn a_file_replaced_by_the_user_is_forgotten() {
    let world = World::new();
    world.manifest(REPO_ENTRY);
    world.sync(Options::default()).unwrap();
    fs::remove_file(world.root().join("models/x/config.json")).unwrap();
    fs::write(world.root().join("models/x/config.json"), b"mine").unwrap();

    world.manifest("");
    let report = world
        .sync(Options {
            prune: true,
            ..Options::default()
        })
        .unwrap();

    assert_eq!(report.pruned, 1);
    assert_eq!(world.read("models/x/config.json"), b"mine");
    let workspace = Workspace::open(world.root(), None).unwrap();
    assert!(Placed::load(&workspace.placed_path()).unwrap().is_empty());
}

#[cfg(unix)]
fn overwrite_in_place(path: &Path, content: &[u8]) {
    use std::os::unix::fs::PermissionsExt as _;
    // What a tool running as root, or one that changes permissions first,
    // does: write into the same inode.
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o444)).unwrap();
}

#[cfg(unix)]
#[test]
fn a_blob_that_was_written_to_is_left_alone_unless_forced() {
    let world = World::new();
    world.manifest(REPO_ENTRY);
    world.sync(Options::default()).unwrap();
    overwrite_in_place(
        &world.root().join("models/vae/x_vae.safetensors"),
        b"WEIGHTS",
    );

    let (result, events, _) = world.sync_with(&[Remote::Dir(world.public())], Options::default());

    let report = result.unwrap();
    assert_eq!(report.damaged, ["models/vae/x_vae.safetensors"]);
    assert_eq!(report.unchanged, 1);
    assert!(events.contains(&"Damaged(\"models/vae/x_vae.safetensors\")".to_owned()));
    assert_eq!(world.read("models/vae/x_vae.safetensors"), b"WEIGHTS");

    // Nor does prune remove a file whose blob was written to.
    world.manifest(&format!(
        "[[model]]\nsource = \"hf:{HUB}:{REPO}\"\nfiles = [{{ path = \"config.json\", dest = \"models/x/\" }}]\n"
    ));
    let pruned = world
        .sync(Options {
            prune: true,
            ..Options::default()
        })
        .unwrap();
    assert_eq!(pruned.kept, ["models/vae/x_vae.safetensors"]);
    assert!(world.exists("models/vae/x_vae.safetensors"));

    world.manifest(REPO_ENTRY);
    let forced = world
        .sync(Options {
            force: true,
            ..Options::default()
        })
        .unwrap();
    assert_eq!((forced.fetched, forced.linked), (1, 1));
    assert_eq!(forced.damaged, Vec::<String>::new());
    assert_eq!(world.read("models/vae/x_vae.safetensors"), b"weights");
    assert!(is_link_to_blob(
        &world,
        "models/vae/x_vae.safetensors",
        b"weights"
    ));
    assert_eq!(world.sync(Options::default()).unwrap().unchanged, 2);
}

#[cfg(unix)]
#[test]
fn a_blob_that_was_only_touched_is_recorded_again() {
    let world = World::new();
    world.manifest(REPO_ENTRY);
    world.sync(Options::default()).unwrap();
    overwrite_in_place(
        &world.root().join("models/vae/x_vae.safetensors"),
        b"weights",
    );

    let report = world.sync(Options::default()).unwrap();

    assert_eq!(report.damaged, Vec::<String>::new());
    assert_eq!(report.unchanged, 2);
    let workspace = Workspace::open(world.root(), None).unwrap();
    let record = Placed::load(&workspace.placed_path()).unwrap();
    let entry = record.get("models/vae/x_vae.safetensors").unwrap();
    let blob = workspace.store.blob_path(&entry.sha256);
    assert!(Stamp::of(&blob).unwrap().unwrap().matches(entry));
}

#[test]
fn a_destination_inside_the_blobs_is_rejected() {
    let world = World::new();
    world.settings("blobs = \"models/.blobs\"\n");
    world.manifest(&format!(
        "[[model]]\nsource = \"hf:{HUB}:{REPO}\"\nfiles = [{{ path = \"config.json\", dest = \"models/.blobs/sha256/x.json\" }}]\n"
    ));

    let error = world.sync(Options::default()).unwrap_err().to_string();

    assert!(error.contains("inside the blobs"), "{error}");
}

#[test]
fn a_dry_run_changes_nothing_in_the_state_directory() {
    let world = World::new();
    world.manifest(REPO_ENTRY);

    world
        .sync(Options {
            dry_run: true,
            ..Options::default()
        })
        .unwrap();

    assert!(!world.exists(".mvault"));
}

#[test]
fn the_same_content_is_fetched_once_and_placed_twice() {
    let world = World::new();
    world.manifest(&format!(
        "[[model]]\nsource = \"hf:{HUB}:{REPO}\"\nfiles = [\n\
         {{ path = \"config.json\", dest = \"a/\" }},\n{{ path = \"config.json\", dest = \"b/copy.json\" }},\n]\n"
    ));

    let report = world.sync(Options::default()).unwrap();

    assert_eq!((report.fetched, report.linked), (1, 2));
    assert!(
        same_file::is_same_file(
            world.root().join("a/config.json"),
            world.root().join("b/copy.json")
        )
        .unwrap()
    );
}

#[test]
fn a_file_without_a_digest_is_fetched_once_for_two_places() {
    let world = World::new();
    world.manifest(&format!(
        "[[model]]\nsource = \"hf:{HUB}:{REPO}\"\nfiles = [\n\
         {{ path = \"config.json\", dest = \"a/\" }},\n{{ path = \"config.json\", dest = \"b/copy.json\" }},\n]\n"
    ));
    world.settings("allow_official = true\n");
    let before = world.upstream_requests();

    let (result, _, _) = world.sync_with(&[], Options::default());

    let report = result.unwrap();
    assert_eq!((report.fetched, report.linked), (1, 2));
    assert_eq!(world.read("a/config.json"), b"{}");
    assert_eq!(world.read("b/copy.json"), b"{}");
    // One request to resolve, one to download.
    assert_eq!(world.upstream_requests(), before + 2);
}

#[test]
fn a_mirror_that_fails_is_passed_over_with_a_warning() {
    let world = World::new();
    world.manifest(REPO_ENTRY);
    let broken = Routes::start();
    let empty = tempfile::tempdir().unwrap();
    let remotes = [
        Remote::parse(&format!("{}/missing", broken.base())).unwrap(),
        Remote::Dir(empty.path().to_owned()),
        Remote::Dir(world.public()),
    ];
    // Answers 500 for everything below this path.
    let failing = crate::testutil::TestServer::start(|_| crate::testutil::Response::status(500));
    let with_failing = [
        Remote::parse(&failing.url("")).unwrap(),
        Remote::Dir(world.public()),
    ];

    let (result, _, warnings) = world.sync_with(&remotes, Options::default());
    assert_eq!(result.unwrap().linked, 2);
    assert_eq!(warnings, Vec::<String>::new());

    fs::remove_dir_all(world.root().join("models")).unwrap();
    fs::remove_file(world.root().join(LOCK_NAME)).unwrap();
    let (result, _, warnings) = world.sync_with(&with_failing, Options::default());
    assert_eq!(result.unwrap().linked, 2);
    assert_ne!(warnings, Vec::<String>::new());
    assert!(
        warnings
            .iter()
            .all(|warning| warning.contains("HTTP status 500")),
        "{warnings:?}"
    );
}

#[test]
fn a_blob_with_other_content_is_rejected() {
    let world = World::new();
    world.manifest(REPO_ENTRY);
    // A copy of the mirror in which one blob was replaced.
    let tampered = world.dir.path().join("tampered");
    let digest = Sha256::of(b"weights");
    let relative = Path::new("blobs").join(BlobStore::relative_path(&digest));
    fs::create_dir_all(tampered.join(&relative).parent().unwrap()).unwrap();
    fs::write(tampered.join(&relative), b"tampered").unwrap();
    for path in [
        format!("snapshots/hf/{HUB}/{REPO}/{COMMIT}.json"),
        format!("snapshots/hf/{HUB}/{REPO}/refs/main"),
    ] {
        fs::create_dir_all(tampered.join(&path).parent().unwrap()).unwrap();
        fs::copy(world.public().join(&path), tampered.join(&path)).unwrap();
    }

    let (result, _, _) = world.sync_with(&[Remote::Dir(tampered)], Options::default());

    let error = result.unwrap_err().to_string();
    assert!(
        error.contains(&format!("blob {digest} has the content of")),
        "{error}"
    );
    assert!(!world.exists("models/vae/x_vae.safetensors"));
    let workspace = Workspace::open(world.root(), None).unwrap();
    assert!(!workspace.store.contains(&Sha256::of(b"tampered")));
    assert!(
        !workspace
            .store
            .partial_path(&digest.to_string())
            .unwrap()
            .exists()
    );
}

#[test]
fn blobs_may_live_outside_the_workspace() {
    let world = World::new();
    world.manifest(REPO_ENTRY);
    let shared = world.dir.path().join("shared/blobs");
    world.settings(&format!(
        "blobs = {}\n",
        toml::Value::String(shared.to_string_lossy().into_owned())
    ));

    world.sync(Options::default()).unwrap();

    assert!(
        shared
            .join(BlobStore::relative_path(&Sha256::of(b"weights")))
            .is_file()
    );
    assert!(!world.exists(".mvault/blobs"));
    assert!(is_link_to_blob(
        &world,
        "models/vae/x_vae.safetensors",
        b"weights"
    ));
}
