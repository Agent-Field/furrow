//! Contract S1–S10: shared timelines and opt-in Git-aware capture through the public interfaces.

use furrow::exchange::{frame, ops};
use furrow::model::{id_hex, ObjectKind, Snapshot, SnapshotTrigger};
use furrow::sealer::Keys;
use furrow::{FurrowRepository, SealOptions};
use furrow_daemon::{router::Router, verbs, Target};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};

fn furrow_exe() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_furrow")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_furrow")))
}

struct Rig {
    _temp: tempfile::TempDir,
    data: PathBuf,
    tree: PathBuf,
}

impl Rig {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let tree = temp.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::create_dir(temp.path().join("home")).unwrap();
        Self {
            _temp: temp,
            data,
            tree,
        }
    }

    fn cli(&self, timeline: &str, args: &[&str]) -> Value {
        let output = Command::new(furrow_exe())
            .env("FURROW_DATA_DIR", &self.data)
            .env("HOME", self._temp.path().join("home"))
            .env("FURROW_NO_DAEMON", "1")
            .args(["--json", "--repo"])
            .arg(&self.tree)
            .args(["--timeline", timeline])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::Deserializer::from_slice(&output.stdout)
            .into_iter::<Value>()
            .map(Result::unwrap)
            .last()
            .unwrap()
    }

    fn seal(&self, timeline: &str) -> String {
        self.cli(timeline, &["hook", "turn-end"])["snapshot"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn repo(&self, timeline: &str) -> FurrowRepository {
        FurrowRepository::open_timeline_in(&self.data, &self.tree, None, Some(timeline)).unwrap()
    }

    fn target(&self, timeline: &str) -> Target {
        Target {
            data_dir: self.data.clone(),
            tree: self.tree.clone(),
            cell_dir: None,
            timeline: Some(timeline.to_owned()),
        }
    }
}

fn disk_bytes(path: &Path) -> u64 {
    fs::read_dir(path)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let m = e.metadata().unwrap();
            if m.is_dir() {
                disk_bytes(&e.path())
            } else {
                m.len()
            }
        })
        .sum()
}

#[test]
fn s1_s2_independent_heads_share_content_on_disk() {
    // S1 and S2 require four independent histories with at most 15% storage overhead.
    let rig = Rig::new();
    let mut state = 7_u64;
    let mut bytes = Vec::new();
    for i in 0..31 {
        let content: Vec<u8> = (0..1024 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        fs::write(rig.tree.join(format!("{i}.bin")), &content).unwrap();
        if i == 0 {
            bytes = content;
        }
    }
    let first = rig.seal("a");
    let one = disk_bytes(&rig.data);
    for id in ["b", "c", "d"] {
        rig.seal(id);
    }
    let four = disk_bytes(&rig.data);
    eprintln!(
        "S2 disk bytes one={one} four={four} ratio={:.4}",
        four as f64 / one as f64
    );
    assert!(four * 100 <= one * 115);
    fs::write(rig.tree.join("0.bin"), vec![9; 1024 * 1024]).unwrap();
    let changed = rig.seal("b");
    let after = disk_bytes(&rig.data);
    eprintln!("S2 changed-file bytes added={}", after - four);
    assert!(after - four < 2 * 1024 * 1024);
    assert_ne!(first, changed);
    assert_eq!(rig.repo("a").status().unwrap().head.unwrap(), first);
    assert_eq!(rig.repo("b").status().unwrap().head.unwrap(), changed);
    assert_eq!(rig.repo("a").timeline(100).unwrap().len(), 1);
    assert_eq!(rig.repo("b").timeline(100).unwrap().len(), 2);
    rig.cli("a", &["rewind", "head", "--yes"]);
    assert_eq!(fs::read(rig.tree.join("0.bin")).unwrap(), bytes);
    assert_eq!(rig.repo("b").status().unwrap().head.unwrap(), changed);
    assert!(!rig.tree.join(".furrow").exists());
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .args(["-c", "core.autocrlf=false"])
        .env("HOME", root)
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Git fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn split_paths(bytes: &[u8]) -> BTreeSet<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8(p.to_vec()).unwrap())
        .collect()
}

fn captured_files(repo: &FurrowRepository, id: &str) -> BTreeSet<String> {
    fn walk(
        store: &furrow::store::ObjectStore,
        id: &furrow::model::ObjectId,
        prefix: &str,
        found: &mut BTreeSet<String>,
    ) {
        furrow::tree::for_each_entry(store, id, |e| {
            let path = format!("{prefix}{}", String::from_utf8_lossy(&e.name));
            if e.kind == furrow::model::EntryKind::Directory {
                walk(store, &e.target.unwrap(), &format!("{path}/"), found);
            } else {
                found.insert(path);
            }
            Ok(())
        })
        .unwrap();
    }
    let store = furrow::store::ObjectStore::open(repo.store_root().to_owned()).unwrap();
    let snap: Snapshot = store
        .read_struct(&furrow::model::parse_id(id).unwrap(), ObjectKind::Snapshot)
        .unwrap();
    let mut found = BTreeSet::new();
    walk(&store, &snap.root_tree, "", &mut found);
    found
}

#[test]
fn s6_s7_s8_git_matrix_and_ignored_restore_protection() {
    // S6 compares capture against real Git, including nested rules and tracked exceptions.
    // S7 protects ignored content from old snapshots, safety seals and directory replacement.
    // G4 reports collapsed omitted groups without counting files or bytes inside them.
    let rig = Rig::new();
    git(&rig.tree, &["init", "-q"]);
    let global = rig._temp.path().join("global-ignore");
    fs::write(&global, "*.global\n").unwrap();
    git(
        &rig.tree,
        &["config", "core.excludesFile", global.to_str().unwrap()],
    );
    fs::write(rig.tree.join(".git/info/exclude"), "*.local\n").unwrap();
    fs::write(
        rig.tree.join(".gitignore"),
        "*.log\n!keep.log\nfoo/\n/anchored\n**/cache/**\nnode_modules/\ndist/\n",
    )
    .unwrap();
    let paths = [
        "ignored.log",
        "keep.log",
        "foo/a",
        "other/foo/a",
        "anchored",
        "other/anchored",
        "a/cache/out",
        "nested/.gitignore",
        "nested/one.tmp",
        "nested/keep.tmp",
        "force.log",
        "x.global",
        "x.local",
        "node_modules/pkg/a",
        "dist/b",
        "dist/tracked",
        "source.txt",
    ];
    for p in paths {
        let p = rig.tree.join(p);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "data").unwrap();
    }
    fs::write(rig.tree.join("nested/.gitignore"), "*.tmp\n!keep.tmp\n").unwrap();
    // The contract's force-added ignored fixture requires a real tracked index entry.
    git(&rig.tree, &["add", "-f", "force.log", "dist/tracked"]);
    let old = rig.seal("a");
    fs::write(rig.tree.join(".furrowpolicy"), "gitignore\n").unwrap();
    let report = rig.cli("a", &["hook", "turn-end"]);
    let head = report["snapshot"].as_str().unwrap();
    let ignored = split_paths(&git(
        &rig.tree,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
        ],
    ));
    let tracked = split_paths(&git(&rig.tree, &["ls-files", "-z"]));
    let captured = captured_files(&rig.repo("a"), head);
    for p in paths {
        assert_eq!(
            captured.contains(p),
            !ignored.contains(p) || tracked.contains(p),
            "{p}"
        );
    }
    assert!(report["ignored"].get("files").is_none());
    assert!(report["ignored"].get("bytes").is_none());
    assert_eq!(report["ignored"]["more"], 0);
    assert!(report["ignored"]["paths"]
        .as_array()
        .unwrap()
        .contains(&json!("dist/")));
    assert!(report["ignored"]["paths"].as_array().unwrap().len() <= 20);
    for p in &ignored {
        fs::write(rig.tree.join(p), "preserved local content").unwrap();
    }
    rig.cli("a", &["rewind", &old, "--yes"]);
    for p in &ignored {
        assert_eq!(
            fs::read_to_string(rig.tree.join(p)).unwrap(),
            "preserved local content",
            "{p}"
        );
    }
    let safety = rig.repo("a").timeline(1).unwrap().remove(0).id;
    let safety_files = captured_files(&rig.repo("a"), &safety);
    assert!(ignored.iter().all(|p| !safety_files.contains(p)));
    // Re-enable the current capture preference after restoring an older policy-free snapshot.
    fs::write(rig.tree.join(".furrowpolicy"), "gitignore\n").unwrap();
    rig.cli("a", &["materialize", "--head", &old]);
    for p in &ignored {
        assert_eq!(
            fs::read_to_string(rig.tree.join(p)).unwrap(),
            "preserved local content"
        );
    }
}

#[test]
fn s6_s10_directive_outside_git_and_legacy_identity_are_unchanged() {
    // S6 makes the directive inert outside Git; S10 retains legacy JSON and identities.
    let rig = Rig::new();
    fs::write(
        rig.tree.join(".furrowpolicy"),
        "gitignore\nexclude secret\n",
    )
    .unwrap();
    fs::write(rig.tree.join("source"), "yes").unwrap();
    fs::write(rig.tree.join("secret"), "no").unwrap();
    let answer = rig.cli("a", &["hook", "turn-end"]);
    assert!(answer.get("ignored").is_none());
    let files = captured_files(&rig.repo("a"), answer["snapshot"].as_str().unwrap());
    assert!(files.contains("source"));
    assert!(!files.contains("secret"));
    let (_, head) = FurrowRepository::attach_and_seal_in(
        &rig.data,
        &rig.tree,
        None,
        SnapshotTrigger::Manual,
        SealOptions::default(),
    )
    .unwrap();
    assert_eq!(
        FurrowRepository::open_composed_in(&rig.data, &rig.tree, None)
            .unwrap()
            .status()
            .unwrap()
            .head,
        Some(id_hex(&head))
    );
    assert!(rig.tree.join(".furrow/workspace-id").exists());
    assert_ne!(rig.repo("a").status().unwrap().head, Some(id_hex(&head)));
}

#[test]
fn s3_daemon_writers_and_one_shot_processes_stress_shared_store() {
    // S3 exercises the daemon's real writer router, concurrent first-open and CLI processes.
    let rig = Rig::new();
    fs::write(rig.tree.join("content"), vec![3; 1024 * 1024]).unwrap();
    let router = Arc::new(Router::new());
    let barrier = Arc::new(Barrier::new(3));
    std::thread::scope(|scope| {
        for id in ["a", "b"] {
            let router = router.clone();
            let barrier = barrier.clone();
            let rig = &rig;
            scope.spawn(move || {
                barrier.wait();
                for turn in 0..30 {
                    router
                        .submit(
                            rig.target(id),
                            verbs::lookup("seal").unwrap(),
                            json!({"turn": turn.to_string()}),
                        )
                        .unwrap();
                }
            });
        }
        barrier.wait();
    });
    let barrier = Arc::new(Barrier::new(3));
    std::thread::scope(|scope| {
        let barrier_a = barrier.clone();
        let rig_a = &rig;
        let router_a = router.clone();
        scope.spawn(move || {
            barrier_a.wait();
            for turn in 30..60 {
                router_a
                    .submit(
                        rig_a.target("a"),
                        verbs::lookup("seal").unwrap(),
                        json!({"turn": turn.to_string()}),
                    )
                    .unwrap();
            }
        });
        let barrier_b = barrier.clone();
        let rig_b = &rig;
        scope.spawn(move || {
            barrier_b.wait();
            for _ in 0..30 {
                rig_b.seal("b");
            }
        });
        barrier.wait();
    });
    router.close();
    for id in ["a", "b"] {
        let repo = rig.repo(id);
        assert_eq!(repo.timeline(100).unwrap().len(), 60);
        assert_eq!(
            repo.materialization(&repo.resolve_snapshot("head").unwrap())
                .unwrap()
                .grade,
            "exact"
        );
    }
}

#[test]
fn s4_s5_export_import_dedup_and_gc_preserve_other_timeline() {
    // S4 transports a complete shared DAG unchanged; S5 retains a sibling's reachable bytes after purge/GC.
    let source = Rig::new();
    fs::write(source.tree.join("shared"), vec![4; 1024 * 1024]).unwrap();
    let first = source.seal("a");
    source.seal("b");
    let keys = Keys {
        cell_key: [0x11; 32],
        dedup: [0x22; 32],
    };
    let outbox = source._temp.path().join("outbox");
    let ledger = "0123456789abcdef";
    let export = ops::export(&source.data, &keys, &first, ledger, &outbox, 8 << 20).unwrap();
    let target = Rig::new();
    fs::write(target.tree.join("shared"), vec![4; 1024 * 1024]).unwrap();
    let sibling = target.seal("existing");
    let before = disk_bytes(&target.data);
    let inbox = target._temp.path().join("inbox");
    fs::create_dir(&inbox).unwrap();
    let mut frames = Vec::new();
    for entry in export["frames"].as_array().unwrap() {
        let path = PathBuf::from(entry["path"].as_str().unwrap());
        let bytes = fs::read(&path).unwrap();
        let (_, objects) = frame::decode(&bytes).unwrap();
        for object in objects {
            fs::write(inbox.join(id_hex(&object.rid)), object.bytes).unwrap();
        }
        frames.push(path);
    }
    ops::import(
        &target.data,
        &keys,
        &first,
        ledger,
        &inbox,
        ops::ImportMode::Primed,
    )
    .unwrap();
    assert!(ops::wanted(&target.data, &keys, &first).unwrap()["want"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(disk_bytes(&target.data) - before < 512 * 1024);
    ops::restore_timeline_head(&target.data, &target.tree, None, &first, Some("imported")).unwrap();
    assert_eq!(
        target.repo("existing").status().unwrap().head.unwrap(),
        sibling
    );
    ops::record_published(&source.data, ledger, &frames).unwrap();
    assert!(
        ops::export(&source.data, &keys, &first, ledger, &outbox, 8 << 20).unwrap()["frames"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    target.repo("imported").forget(true).unwrap();
    let mut remaining = target.repo("existing");
    remaining.gc(false).unwrap();
    assert_eq!(
        remaining
            .materialization(&remaining.resolve_snapshot("head").unwrap())
            .unwrap()
            .grade,
        "exact"
    );
    fs::write(target.tree.join("shared"), "damage").unwrap();
    remaining
        .rewind(&remaining.resolve_snapshot("head").unwrap(), &[], false)
        .unwrap();
    assert_eq!(
        fs::read(target.tree.join("shared")).unwrap(),
        vec![4; 1024 * 1024]
    );
}

#[test]
fn s1_fork_merge_and_rebind_select_only_the_named_timeline() {
    // S1 keeps forks, merge publication and move bindings independent in a shared store.
    let mut rig = Rig::new();
    fs::write(rig.tree.join("file"), "base").unwrap();
    rig.seal("a");
    let sibling = rig.seal("b");
    let destination = rig._temp.path().join("fork");
    let mut repo = rig.repo("a");
    repo.fork("job", &destination).unwrap();
    assert!(rig.repo("b").forks().unwrap().is_empty());
    fs::write(destination.join("file"), "merged").unwrap();
    // I3 uses the native shell's successful command so the same merge check runs on Windows.
    let check = if cfg!(windows) { "exit /b 0" } else { "true" };
    let merged = repo.merge("job", Some(check), false).unwrap();
    assert!(merged.result_snapshot.is_some());
    assert_eq!(fs::read_to_string(rig.tree.join("file")).unwrap(), "merged");
    assert_eq!(rig.repo("b").status().unwrap().head.unwrap(), sibling);
    repo.remove_fork("job", false).unwrap();
    drop(repo);
    let old = rig.tree.clone();
    rig.tree = rig._temp.path().join("moved");
    fs::rename(&old, &rig.tree).unwrap();
    rig.cli("a", &["rebind", "--from", old.to_str().unwrap()]);
    assert!(rig.repo("a").status().unwrap().head.is_some());
    assert!(FurrowRepository::open_timeline_in(&rig.data, &rig.tree, None, Some("b")).is_err());
    ops::rebind_timeline(&rig.data, &old, &rig.tree, Some("b")).unwrap();
    assert_eq!(rig.repo("b").status().unwrap().head.unwrap(), sibling);
}

#[test]
fn s7_ignored_children_survive_ancestor_removal_and_replacement() {
    // S7 forbids recursively deleting ignored children when an old tree lacks their parent.
    let rig = Rig::new();
    git(&rig.tree, &["init", "-q"]);
    fs::write(rig.tree.join(".gitignore"), "nested/dist/\n").unwrap();
    let absent = rig.seal("a");
    fs::write(rig.tree.join(".furrowpolicy"), "gitignore\n").unwrap();
    fs::create_dir_all(rig.tree.join("nested/dist")).unwrap();
    fs::write(rig.tree.join("nested/dist/local"), "keep").unwrap();
    fs::write(rig.tree.join("nested/ordinary"), "remove").unwrap();
    rig.cli("a", &["rewind", &absent, "--yes"]);
    assert_eq!(
        fs::read_to_string(rig.tree.join("nested/dist/local")).unwrap(),
        "keep"
    );
    assert!(!rig.tree.join("nested/ordinary").exists());
}

#[test]
fn s7_sqlite_and_hardlink_restore_never_replace_ignored_files() {
    // S7 applies to logical SQLite restores and hard-link reconstruction as well as ordinary files.
    let rig = Rig::new();
    git(&rig.tree, &["init", "-q"]);
    fs::write(rig.tree.join(".gitignore"), "dist/\n").unwrap();
    fs::create_dir(rig.tree.join("dist")).unwrap();
    fs::write(rig.tree.join("source"), "base").unwrap();
    fs::hard_link(rig.tree.join("source"), rig.tree.join("dist/link")).unwrap();
    let database = rig.tree.join("dist/data.sqlite");
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute_batch("CREATE TABLE item (value TEXT); INSERT INTO item VALUES ('old');")
        .unwrap();
    let old = rig.seal("a");
    let metadata = fs::metadata(rig.tree.join("dist/link")).unwrap();
    fs::remove_file(rig.tree.join("dist/link")).unwrap();
    fs::write(rig.tree.join("dist/link"), "safe").unwrap();
    filetime::set_file_mtime(
        rig.tree.join("dist/link"),
        filetime::FileTime::from_last_modification_time(&metadata),
    )
    .unwrap();
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute("UPDATE item SET value='local'", [])
        .unwrap();
    fs::write(rig.tree.join(".furrowpolicy"), "gitignore\n").unwrap();
    fs::write(rig.tree.join("source"), "damage").unwrap();
    rig.cli("a", &["rewind", &old, "--yes", "--sqlite-consistent"]);
    assert_eq!(fs::read_to_string(rig.tree.join("source")).unwrap(), "base");
    assert_eq!(
        fs::read_to_string(rig.tree.join("dist/link")).unwrap(),
        "safe"
    );
    let value: String = rusqlite::Connection::open(&database)
        .unwrap()
        .query_row("SELECT value FROM item", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "local");
}

#[test]
fn s5_s7_shrink_cannot_remove_ignored_undo_bytes() {
    // S5 and S7 keep shrink from claiming undo protection for paths the active policy leaves out.
    let rig = Rig::new();
    git(&rig.tree, &["init", "-q"]);
    fs::write(rig.tree.join(".gitignore"), "node_modules/\n").unwrap();
    fs::create_dir(rig.tree.join("node_modules")).unwrap();
    fs::write(rig.tree.join("node_modules/local"), "keep").unwrap();
    let sibling = rig.seal("b");
    fs::write(rig.tree.join(".furrowpolicy"), "gitignore\n").unwrap();
    assert!(furrow::shrink::discover(&rig.tree, &[PathBuf::from("node_modules")]).is_err());
    assert!(furrow::shrink::discover(&rig.tree, &[])
        .unwrap()
        .candidates
        .is_empty());
    let mut repo = rig.repo("b");
    repo.gc(false).unwrap();
    assert_eq!(
        repo.materialization(&furrow::model::parse_id(&sibling).unwrap())
            .unwrap()
            .grade,
        "exact"
    );
    assert_eq!(
        fs::read_to_string(rig.tree.join("node_modules/local")).unwrap(),
        "keep"
    );
}

#[test]
fn s3_stalled_writer_times_out_with_a_bounded_retry_message() {
    // S3 bounds catalog contention and reports the wait rather than leaking a SQLite lock failure.
    let rig = Rig::new();
    fs::write(rig.tree.join("file"), "data").unwrap();
    rig.seal("a");
    let connection = rusqlite::Connection::open(rig.data.join("store-v1/catalog.sqlite3")).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = std::time::Instant::now();
    let output = Command::new(furrow_exe())
        .env("FURROW_DATA_DIR", &rig.data)
        .env("HOME", rig._temp.path().join("home"))
        .args(["--json", "--repo"])
        .arg(&rig.tree)
        .args(["--timeline", "b", "hook", "turn-end"])
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        error.contains("waiting 30 seconds") && error.contains("retry"),
        "{error}"
    );
    assert!(!error.contains("database is locked"), "{error}");
    assert!(elapsed >= std::time::Duration::from_secs(29));
    assert!(elapsed < std::time::Duration::from_secs(45), "{elapsed:?}");
    connection.execute_batch("ROLLBACK").unwrap();
    rig.seal("b");
}

#[test]
fn g4_ignored_nested_repository_and_empty_directory_are_collapsed() {
    // G4 reports ignored nested repositories and empty directories without traversing them.
    let rig = Rig::new();
    git(&rig.tree, &["init", "-q"]);
    fs::write(rig.tree.join(".gitignore"), "dist/\nempty/\n").unwrap();
    fs::write(rig.tree.join(".furrowpolicy"), "gitignore\n").unwrap();
    fs::create_dir(rig.tree.join("dist")).unwrap();
    fs::create_dir(rig.tree.join("empty")).unwrap();
    git(&rig.tree.join("dist"), &["init", "-q"]);
    fs::write(rig.tree.join("dist/output"), "omitted").unwrap();
    let report = rig.cli("a", &["hook", "turn-end"]);
    assert_eq!(report["ignored"]["more"], 0);
    assert!(report["ignored"].get("files").is_none());
    assert!(report["ignored"].get("bytes").is_none());
    assert_eq!(report["ignored"]["paths"], json!(["dist/", "empty/"]));
    let files = captured_files(&rig.repo("a"), report["snapshot"].as_str().unwrap());
    assert!(!files.iter().any(|p| p.starts_with("dist/")));
}

#[test]
fn s1_fork_commands_keep_the_forks_own_identity() {
    // S1 gives commands in an isolated fork that fork's independent timeline and leaves sibling heads alone.
    let rig = Rig::new();
    fs::write(rig.tree.join("file"), "base").unwrap();
    rig.seal("a");
    let sibling = rig.seal("b");
    for verb in ["fork", "run", "exec"] {
        let name = format!("{verb}-job");
        let destination = rig._temp.path().join(&name);
        let mut command = Command::new(furrow_exe());
        command
            .env("FURROW_DATA_DIR", &rig.data)
            .env("HOME", rig._temp.path().join("home"))
            .env("FURROW_TIMELINE", "a")
            .env("FURROW_DISABLE_NAMESPACES", "1")
            .arg("--repo")
            .arg(&rig.tree);
        if verb == "exec" {
            command.args(["exec", "--fork", &name]);
        } else {
            command
                .args([verb, &name, "--destination"])
                .arg(&destination);
        }
        let output = command
            .arg("--")
            .arg(furrow_exe())
            .args(["--json", "snap", "-m", "child command"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{verb}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let parent = rig.repo("a");
        let fork = parent
            .forks()
            .unwrap()
            .into_iter()
            .find(|f| f.name == name)
            .unwrap();
        let child = FurrowRepository::open_composed_in(&rig.data, &fork.destination, None).unwrap();
        assert!(child
            .timeline(10)
            .unwrap()
            .iter()
            .any(|s| s.label.as_deref() == Some("child command")));
        assert_ne!(child.status().unwrap().head, parent.status().unwrap().head);
        assert_eq!(
            rig.repo("b").status().unwrap().head.as_deref(),
            Some(sibling.as_str())
        );
    }
}

#[test]
fn g1_g3_delta_only_captures_named_paths_and_reloads_rules() {
    // G1 reloads replaced index/rule identities; G3 leaves unrelated changed content for a full seal.
    let rig = Rig::new();
    git(&rig.tree, &["init", "-q"]);
    fs::write(rig.tree.join(".furrowpolicy"), "gitignore\n").unwrap();
    fs::write(rig.tree.join(".gitignore"), "*.log\ndist/\n").unwrap();
    fs::write(rig.tree.join("force.log"), "tracked").unwrap();
    fs::write(rig.tree.join("other.log"), "ignored").unwrap();
    fs::create_dir_all(rig.tree.join("dist/inner")).unwrap();
    fs::write(rig.tree.join("dist/inner/tracked"), "tracked").unwrap();
    fs::write(rig.tree.join("dist/inner/ignored"), "ignored").unwrap();
    fs::write(rig.tree.join("source"), "original").unwrap();
    fs::write(rig.tree.join("unrelated"), "original").unwrap();
    git(&rig.tree, &["add", "-f", "force.log", "dist/inner/tracked"]);
    git(&rig.tree, &["update-index", "--index-version=4"]);
    rig.seal("a");
    let mut repo = rig.repo("a");
    let first = repo.snapshot(None, SnapshotTrigger::Watcher).unwrap();
    let first_files = captured_files(&repo, &id_hex(&first));
    assert!(first_files.contains("dist/inner/tracked"));
    assert!(!first_files.contains("dist/inner/ignored"));
    assert!(first_files.contains("force.log"));
    assert!(!first_files.contains("other.log"));
    fs::write(rig.tree.join("source"), "named change").unwrap();
    fs::write(rig.tree.join("unrelated"), "unlisted change").unwrap();
    let delta = repo
        .snapshot_changed_paths(None, SnapshotTrigger::Watcher, &[PathBuf::from("source")])
        .unwrap();
    let store = furrow::store::ObjectStore::open(repo.store_root().to_owned()).unwrap();
    let previous: Snapshot = store.read_struct(&first, ObjectKind::Snapshot).unwrap();
    let current: Snapshot = store.read_struct(&delta, ObjectKind::Snapshot).unwrap();
    let entries = |snapshot: &Snapshot| {
        let mut result = std::collections::BTreeMap::new();
        furrow::tree::for_each_entry(&store, &snapshot.root_tree, |entry| {
            result.insert(entry.name.clone(), entry.target);
            Ok(())
        })
        .unwrap();
        result
    };
    assert_eq!(
        entries(&previous)[b"unrelated".as_slice()],
        entries(&current)[b"unrelated".as_slice()]
    );
    assert_ne!(
        entries(&previous)[b"source".as_slice()],
        entries(&current)[b"source".as_slice()]
    );
    assert_eq!(previous.ignored, current.ignored);
    // G2 follows only existing indexed paths inside pruned directories, including staged-but-deleted files.
    fs::remove_file(rig.tree.join("dist/inner/tracked")).unwrap();
    let deleted = repo.snapshot(None, SnapshotTrigger::Watcher).unwrap();
    assert!(!captured_files(&repo, &id_hex(&deleted)).contains("dist/inner/tracked"));
    git(&rig.tree, &["rm", "--cached", "force.log"]);
    git(&rig.tree, &["add", "-f", "other.log"]);
    let swapped = repo.snapshot(None, SnapshotTrigger::Watcher).unwrap();
    let files = captured_files(&repo, &id_hex(&swapped));
    assert!(!files.contains("force.log"));
    assert!(files.contains("other.log"));
    let previous_time = filetime::FileTime::from_last_modification_time(
        &fs::metadata(rig.tree.join(".gitignore")).unwrap(),
    );
    fs::write(rig.tree.join("replacement"), "source\n*.log").unwrap();
    filetime::set_file_mtime(rig.tree.join("replacement"), previous_time).unwrap();
    fs::rename(rig.tree.join("replacement"), rig.tree.join(".gitignore")).unwrap();
    let after = repo
        .snapshot_changed_paths(None, SnapshotTrigger::Watcher, &[PathBuf::from("source")])
        .unwrap();
    assert!(!captured_files(&repo, &id_hex(&after)).contains("source"));
    fs::write(rig.tree.join(".git/index"), "bad").unwrap();
    let missing = repo.snapshot(None, SnapshotTrigger::Watcher).unwrap();
    assert!(!captured_files(&repo, &id_hex(&missing)).contains("other.log"));
    fs::remove_file(rig.tree.join(".git/index")).unwrap();
    repo.snapshot(None, SnapshotTrigger::Watcher).unwrap();
}

#[test]
fn g1_user_config_xdg_linked_worktree_nested_repo_and_no_git_binary() {
    // G1 evaluates user includes, XDG defaults, linked worktrees and nested scopes with no Git binary.
    let rig = Rig::new();
    git(&rig.tree, &["init", "-q"]);
    fs::write(rig.tree.join(".furrowpolicy"), "gitignore\n").unwrap();
    fs::write(rig.tree.join(".gitignore"), "*.outer\nnested/\n").unwrap();
    fs::write(rig.tree.join("kept.outer"), "tracked").unwrap();
    git(&rig.tree, &["add", "-f", "kept.outer"]);
    let nested = rig.tree.join("nested");
    fs::create_dir(&nested).unwrap();
    git(&nested, &["init", "-q"]);
    fs::write(nested.join(".gitignore"), "*.inner\n").unwrap();
    fs::write(nested.join("keep.outer"), "nested scope").unwrap();
    fs::write(nested.join("omit.inner"), "nested ignore").unwrap();
    git(
        &rig.tree,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,1111111111111111111111111111111111111111,nested",
        ],
    );
    let home = rig._temp.path().join("home");
    fs::create_dir_all(home.join(".config/git")).unwrap();
    fs::write(home.join(".config/git/ignore"), "*.global\n").unwrap();
    fs::write(rig.tree.join("omit.global"), "xdg ignored").unwrap();
    fs::write(rig.tree.join("omit.local"), "info ignored").unwrap();
    fs::write(rig.tree.join(".git/info/exclude"), "*.local\n").unwrap();
    let common = rig._temp.path().join("common");
    fs::rename(rig.tree.join(".git"), &common).unwrap();
    let linked = common.join("worktrees/linked");
    fs::create_dir_all(&linked).unwrap();
    fs::copy(common.join("index"), linked.join("index")).unwrap();
    fs::write(linked.join("commondir"), "../..\n").unwrap();
    fs::write(linked.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(
        rig.tree.join(".git"),
        format!("gitdir: {}\n", linked.display()),
    )
    .unwrap();
    let capture = || {
        let output = Command::new(furrow_exe())
            .env("FURROW_DATA_DIR", &rig.data)
            .env("HOME", &home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env("PATH", rig._temp.path().join("no-executables"))
            .env("FURROW_NO_DAEMON", "1")
            .args(["--json", "--repo"])
            .arg(&rig.tree)
            .args(["--timeline", "a", "hook", "turn-end"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        captured_files(&rig.repo("a"), value["snapshot"].as_str().unwrap())
    };
    let files = capture();
    assert!(files.contains("kept.outer"));
    assert!(files.contains("nested/keep.outer"));
    assert!(!files.contains("nested/omit.inner"));
    assert!(!files.contains("omit.global"));
    assert!(!files.contains("omit.local"));
    fs::write(
        home.join(".gitconfig"),
        "[include]\npath = included-config\n",
    )
    .unwrap();
    fs::write(
        home.join("included-config"),
        "[core]\nexcludesFile = ~/.user-ignore\n",
    )
    .unwrap();
    fs::write(home.join(".user-ignore"), "kept.user\n").unwrap();
    fs::write(rig.tree.join("kept.user"), "user ignored").unwrap();
    assert!(!capture().contains("kept.user"));
}

#[test]
fn g4_omitted_groups_are_bounded_and_explicit_exclusions_are_absent() {
    // G4 reports at most twenty collapsed groups and the remaining group count without file totals.
    let rig = Rig::new();
    git(&rig.tree, &["init", "-q"]);
    fs::write(
        rig.tree.join(".furrowpolicy"),
        "gitignore\nexclude private\n",
    )
    .unwrap();
    fs::write(rig.tree.join(".gitignore"), "out*/\nprivate/\n").unwrap();
    fs::create_dir(rig.tree.join("private")).unwrap();
    for i in 0..25 {
        let path = rig.tree.join(format!("out{i:02}"));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("content"), "irrelevant").unwrap();
    }
    let report = rig.cli("a", &["hook", "turn-end"]);
    let paths = report["ignored"]["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 20);
    assert_eq!(paths[0], "out00/");
    assert_eq!(paths[19], "out19/");
    assert_eq!(report["ignored"]["more"], 5);
    assert_eq!(report["ignored"].as_object().unwrap().len(), 2);
}

#[test]
fn g1_git_control_paths_and_case_folded_tracked_files_are_kept() {
    // G1 matches Git's untracked namespace and honors tracked exceptions with core.ignoreCase.
    let rig = Rig::new();
    git(&rig.tree, &["init", "-q"]);
    fs::write(rig.tree.join("tracked.log"), "tracked").unwrap();
    git(&rig.tree, &["add", "tracked.log"]);
    let config = rig.tree.join(".git/config");
    let mut contents = fs::read_to_string(&config).unwrap();
    contents.push_str("\n[core]\nignoreCase = true\n");
    fs::write(config, contents).unwrap();
    fs::rename(rig.tree.join("tracked.log"), rig.tree.join("TRACKED.log")).unwrap();
    fs::write(rig.tree.join(".gitignore"), ".git\n*.log\n").unwrap();
    fs::write(rig.tree.join(".furrowpolicy"), "gitignore\n").unwrap();
    let ignored = split_paths(&git(
        &rig.tree,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
        ],
    ));
    assert!(!ignored.contains("TRACKED.log"));
    assert!(!ignored
        .iter()
        .any(|p| p == ".git" || p.starts_with(".git/")));
    let head = rig.seal("a");
    let files = captured_files(&rig.repo("a"), &head);
    assert!(files.contains(".git/HEAD"));
    assert!(files.contains("TRACKED.log"));
}

#[test]
fn g1_s10_composed_state_is_outside_the_projects_git_ignore_scope() {
    // G1 applies Git rules to the physical project; S10 preserves composed-state capture and explicit exclusions.
    let rig = Rig::new();
    git(&rig.tree, &["init", "-q"]);
    fs::write(rig.tree.join(".gitignore"), ".cell/\n").unwrap();
    let overlay = rig._temp.path().join("overlay");
    fs::create_dir(&overlay).unwrap();
    fs::write(
        overlay.join(".furrowpolicy"),
        "gitignore\nexclude .cell/private\n",
    )
    .unwrap();
    fs::write(overlay.join("state"), "agent continuation").unwrap();
    fs::write(overlay.join("private"), "withheld").unwrap();
    let (repo, head) = FurrowRepository::attach_and_seal_timeline_in(
        &rig.data,
        &rig.tree,
        Some("a"),
        None,
        SnapshotTrigger::Watcher,
        SealOptions {
            overlay: Some(overlay),
            changed: None,
        },
    )
    .unwrap();
    let files = captured_files(&repo, &id_hex(&head));
    assert!(files.contains(".cell/state"));
    assert!(!files.contains(".cell/private"));
}

#[test]
fn g1_g2_malformed_index_names_cannot_escape_or_list_an_ignored_directory() {
    // G1 grants no tracked exception to malformed names; G2 follows only safe, existing indexed paths.
    let rig = Rig::new();
    git(&rig.tree, &["init", "-q"]);
    fs::create_dir_all(rig.tree.join("dist/xx/yy")).unwrap();
    fs::create_dir_all(rig.tree.join("dist/01234567890")).unwrap();
    fs::write(rig.tree.join("dist/xx/yy/secret"), "tracked fixture").unwrap();
    fs::write(rig._temp.path().join("secret"), "outside the project").unwrap();
    git(&rig.tree, &["add", "dist/xx/yy/secret"]);
    fs::write(rig.tree.join(".gitignore"), "dist/\n").unwrap();
    fs::write(rig.tree.join(".furrowpolicy"), "gitignore\n").unwrap();
    let index_path = rig.tree.join(".git/index");
    let original = fs::read(&index_path).unwrap();
    let needle = b"dist/xx/yy/secret";
    let position = original
        .windows(needle.len())
        .position(|p| p == needle)
        .unwrap();
    for invalid in [
        b"dist/../../secret".as_slice(),
        b"dist/01234567890/".as_slice(),
    ] {
        assert_eq!(needle.len(), invalid.len());
        let mut bytes = original.clone();
        bytes[position..position + needle.len()].copy_from_slice(invalid);
        // Git's null-checksum representation lets the reader reach the deliberately malformed name.
        let checksum = bytes.len() - 20;
        bytes[checksum..].fill(0);
        fs::write(&index_path, bytes).unwrap();
        let head = rig.seal("a");
        let files = captured_files(&rig.repo("a"), &head);
        assert!(!files.iter().any(|p| p.starts_with("dist/")), "{files:?}");
    }
    assert_eq!(
        fs::read_to_string(rig._temp.path().join("secret")).unwrap(),
        "outside the project"
    );
}

#[test]
fn g1_g2_s7_sparse_indexes_keep_tracked_working_bytes_without_ignored_siblings() {
    // G1 reads sparse index trees in process; G2 follows their tracked names; S7 keeps ignored siblings.
    for packed in [false, true] {
        let rig = Rig::new();
        git(&rig.tree, &["init", "-q"]);
        fs::create_dir_all(rig.tree.join("dist/deep")).unwrap();
        fs::create_dir(rig.tree.join("keep")).unwrap();
        fs::write(rig.tree.join("keep/source"), "source").unwrap();
        fs::write(rig.tree.join("dist/a.log"), "indexed a").unwrap();
        fs::write(rig.tree.join("dist/deep/b.log"), "indexed b").unwrap();
        fs::write(rig.tree.join(".gitignore"), "dist/\n*.log\n").unwrap();
        git(&rig.tree, &["add", "-f", ".gitignore", "keep", "dist"]);
        git(
            &rig.tree,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-qm",
                "sparse fixture",
            ],
        );
        if packed {
            git(&rig.tree, &["repack", "-adq"]);
        }
        git(
            &rig.tree,
            &["sparse-checkout", "init", "--cone", "--sparse-index"],
        );
        git(&rig.tree, &["sparse-checkout", "set", "keep"]);
        let sparse = git(&rig.tree, &["ls-files", "--sparse", "-s"]);
        let sparse = String::from_utf8(sparse).unwrap();
        assert!(
            sparse
                .lines()
                .any(|line| line.starts_with("040000 ") && line.ends_with("\tdist/")),
            "{sparse}"
        );
        fs::create_dir_all(rig.tree.join("dist/deep")).unwrap();
        fs::write(rig.tree.join("dist/a.log"), "working a").unwrap();
        fs::write(rig.tree.join("dist/deep/b.log"), "working b").unwrap();
        fs::write(rig.tree.join("dist/untracked"), "ignored local output").unwrap();
        fs::write(rig.tree.join("dist/deep/untracked"), "deep ignored output").unwrap();
        fs::write(rig.tree.join(".furrowpolicy"), "gitignore\n").unwrap();
        let head = rig.seal("a");
        let files = captured_files(&rig.repo("a"), &head);
        assert!(files.contains("dist/a.log"));
        assert!(files.contains("dist/deep/b.log"));
        assert!(!files.contains("dist/untracked"));
        assert!(!files.contains("dist/deep/untracked"));
        fs::write(rig.tree.join("dist/a.log"), "later working change").unwrap();
        rig.cli("a", &["rewind", &head, "--yes"]);
        assert_eq!(
            fs::read_to_string(rig.tree.join("dist/a.log")).unwrap(),
            "working a"
        );
        assert_eq!(
            fs::read_to_string(rig.tree.join("dist/untracked")).unwrap(),
            "ignored local output"
        );
    }
}

#[test]
fn g1_s6_tracked_descendants_do_not_reinclude_an_ignored_replacement_file() {
    // G1/S6 use tracked exceptions for directories only when the indexed descendant can exist beneath them.
    let rig = Rig::new();
    git(&rig.tree, &["init", "-q"]);
    fs::create_dir(rig.tree.join("generated")).unwrap();
    fs::write(rig.tree.join("generated/tracked"), "tracked").unwrap();
    git(&rig.tree, &["add", "generated/tracked"]);
    fs::remove_dir_all(rig.tree.join("generated")).unwrap();
    fs::write(rig.tree.join("generated"), "ignored replacement").unwrap();
    fs::write(rig.tree.join(".gitignore"), "generated\n").unwrap();
    fs::write(rig.tree.join(".furrowpolicy"), "gitignore\n").unwrap();
    let ignored = split_paths(&git(
        &rig.tree,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
        ],
    ));
    assert!(ignored.contains("generated"));
    let head = rig.seal("a");
    assert!(!captured_files(&rig.repo("a"), &head).contains("generated"));
}

#[test]
fn g1_s1_s7_rewind_removes_a_tracked_only_directory_matching_an_ignore_rule() {
    // S1/S7 preserve rewind behavior when a matching directory contains only tracked files, with no ignored data.
    let rig = Rig::new();
    git(&rig.tree, &["init", "-q"]);
    fs::write(rig.tree.join(".gitignore"), "dist/\n").unwrap();
    fs::write(rig.tree.join(".furrowpolicy"), "gitignore\n").unwrap();
    let old = rig.seal("a");
    fs::create_dir(rig.tree.join("dist")).unwrap();
    fs::write(rig.tree.join("dist/tracked"), "tracked").unwrap();
    git(&rig.tree, &["add", "-f", "dist/tracked"]);
    assert!(split_paths(&git(
        &rig.tree,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard"
        ]
    ))
    .is_empty());
    rig.seal("a");
    rig.cli("a", &["rewind", &old, "--yes"]);
    assert!(!rig.tree.join("dist").exists());
}
