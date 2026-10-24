//! P1/P3 exercise the actual local protocol, including abrupt process termination.

use assert_cmd::Command;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
    socket: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a"), b"first").unwrap();
        Self {
            root,
            data: temp.path().join("data"),
            socket: temp.path().join("s"),
            _temp: temp,
        }
    }

    fn cli(&self) -> Command {
        let mut command = Command::cargo_bin("furrow").unwrap();
        command
            .env("FURROW_DATA_DIR", &self.data)
            .env("FURROW_NO_DAEMON", "1")
            .args(["--json", "--timeline", "chat", "--repo"])
            .arg(&self.root);
        command
    }

    fn run(&self, arguments: &[&str]) -> Value {
        let output = self
            .cli()
            .args(arguments)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::Deserializer::from_slice(&output)
            .into_iter::<Value>()
            .last()
            .unwrap()
            .unwrap()
    }

    fn start(&self, fault: Option<&str>) -> Child {
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("furrow"));
        command
            .args(["serve", "--socket"])
            .arg(&self.socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(fault) = fault {
            command.env("FURROW_FAULT", fault);
        }
        let mut child = command.spawn().unwrap();
        let start = Instant::now();
        loop {
            if furrow_daemon::client::connect(&self.socket).is_ok() {
                return child;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "daemon startup timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn request(&self, verb: &str, args: Value) -> anyhow::Result<Value> {
        let stream = furrow_daemon::client::connect(&self.socket)?;
        furrow_daemon::client::exchange(
            stream,
            &json!({
                "V": 1, "id": 1, "verb": verb,
                "target": {"data_dir": self.data, "tree": self.root, "timeline": "chat"}, "args": args
            }),
        )
    }
}

fn wait_dead(child: &mut Child) {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                !status.success(),
                "fault injection must stop the process abruptly"
            );
            return;
        }
        if start.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            panic!("fault injection did not stop the process");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn p1_c28_daemon_death_at_each_capture_boundary_preserves_completed_captures() {
    // P1/C28: kill at every pack/capture/publication boundary, then recover and capture again.
    for step in [
        "seal.pack-header",
        "seal.pack-payload",
        "seal.pack-record",
        "seal.captured",
        "seal.packed",
        "seal.committed",
        "seal.ref-written",
        "seal.logged",
        "seal.indexed",
    ] {
        let f = Fixture::new();
        let first = f.run(&["hook", "turn-end", "--durable"])["snapshot"].clone();
        fs::write(f.root.join("a"), b"second").unwrap();
        let mut child = f.start(Some(step));
        assert!(
            f.request("seal", json!({"changed": ["a"]})).is_err(),
            "{step}"
        );
        wait_dead(&mut child);
        let head = f.run(&["status"])["head"].clone();
        if ["seal.ref-written", "seal.logged", "seal.indexed"].contains(&step) {
            assert_ne!(head, first, "{step}: a complete reference was written");
        } else {
            assert_eq!(head, first, "{step}");
        }
        f.run(&["rewind", head.as_str().unwrap(), "--yes"]);
        let expected = if head == first {
            b"first".as_slice()
        } else {
            b"second".as_slice()
        };
        assert_eq!(fs::read(f.root.join("a")).unwrap(), expected, "{step}");
        fs::write(f.root.join("a"), b"third").unwrap();
        f.run(&["hook", "turn-end", "--changed", "a", "--durable"]);
    }
}

#[test]
fn p1_c28_death_at_each_group_commit_boundary_loses_no_acknowledged_capture() {
    // P1/C28: process death during any durability step preserves the earlier capture acknowledgement.
    for step in [
        "group.packs",
        "group.refs",
        "group.frontier-written",
        "group.frontier-synced",
        "group.frontier-renamed",
        "group.frontier",
    ] {
        let f = Fixture::new();
        f.run(&["hook", "turn-end", "--durable"]);
        fs::write(f.root.join("a"), b"second").unwrap();
        let mut child = f.start(Some(step));
        let second = f.request("seal", json!({"changed": ["a"]})).unwrap()["snapshot"].clone();
        let _ = f.request("sync", json!({}));
        wait_dead(&mut child);
        assert_eq!(f.run(&["status"])["head"], second, "{step}");
    }
}

#[test]
fn p1_periodic_group_commit_and_explicit_sync_cover_the_returned_snapshot() {
    // P1: a capture returns pending, then the daemon advances the durable frontier without a new request.
    let f = Fixture::new();
    let mut child = f.start(None);
    let sealed = f.request("seal", json!({})).unwrap();
    assert_eq!(sealed["durable"], false);
    let workspace = furrow::repository::timeline_workspace_id("chat").unwrap();
    let reference = f
        .data
        .join("store-v1/workspaces")
        .join(&workspace)
        .join("refs.log");
    let expected = fs::metadata(&reference).unwrap().len();
    let start = Instant::now();
    loop {
        let frontier: Value =
            serde_json::from_slice(&fs::read(f.data.join("store-v1/group-commit.json")).unwrap())
                .unwrap();
        if frontier["refs"][&workspace] == expected {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "group commit failed to advance"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(f.request("sync", json!({})).unwrap()["durable"], true);
    assert_eq!(
        f.request("seal", json!({"changed": [], "durable": true}))
            .unwrap()["durable"],
        true
    );
    let stream = furrow_daemon::client::connect(&f.socket).unwrap();
    furrow_daemon::client::exchange(stream, &json!({"V":1,"id":1,"verb":"shutdown"})).unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn p3_cli_plan_is_accepted_by_daemon_and_a_delta_contains_only_named_candidates() {
    // P3/C9: the CLI's persisted token has the same meaning on the daemon wire.
    let f = Fixture::new();
    let plan = f.run(&["capture-plan", "--changed", "a"]);
    assert_eq!(plan["full"], true);
    let mut child = f.start(None);
    f.request("seal", json!({"plan": plan["token"], "durable": true}))
        .unwrap();
    let plan = f
        .request("capture-plan", json!({"changed": ["a"]}))
        .unwrap();
    assert_eq!(plan["full"], false);
    assert_eq!(plan["paths"].as_array().unwrap().len(), 1);
    fs::write(f.root.join("a"), b"changed after screening").unwrap();
    assert!(f.request("seal", json!({"plan":plan["token"]})).is_err());
    let stream = furrow_daemon::client::connect(&f.socket).unwrap();
    furrow_daemon::client::exchange(stream, &json!({"V":1,"id":1,"verb":"shutdown"})).unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn p1_spawned_turn_end_returns_while_its_automatic_daemon_stays_alive() {
    // P1/P2: starting the persistent engine cannot retain a spawned caller's output pipes or console.
    let f = Fixture::new();
    let binary = assert_cmd::cargo::cargo_bin("furrow");
    let mut command = std::process::Command::new(binary);
    command
        .env("FURROW_DATA_DIR", &f.data)
        .env("FURROW_SOCKET", &f.socket)
        .env_remove("FURROW_NO_DAEMON")
        .args(["--json", "--timeline", "chat", "--repo"])
        .arg(&f.root)
        .args(["hook", "turn-end"]);
    let (send, receive) = std::sync::mpsc::channel();
    let caller = std::thread::spawn(move || {
        let _ = send.send(command.output());
    });
    let result = receive.recv_timeout(Duration::from_secs(20));
    let timed_out = result.is_err();
    let output = match result {
        Ok(output) => output.unwrap(),
        Err(_) => {
            let _ = f.request("shutdown", json!({}));
            receive
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
        }
    };
    let _ = f.request("shutdown", json!({}));
    caller.join().unwrap();
    assert!(!timed_out, "the caller retained daemon output handles");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let capture: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(capture["durable"], false);
}

#[test]
fn p1_p2_live_daemon_recovers_a_one_shot_writers_torn_pack_before_appending() {
    // P1/P2/C28/S3: a cached store must repair a foreign writer's incomplete suffix without losing an acknowledgement.
    for boundary in ["seal.pack-header", "seal.pack-payload", "seal.pack-record"] {
        let f = Fixture::new();
        f.run(&["hook", "turn-end", "--durable"]);
        let mut daemon = f.start(None);
        let completed = f.request("seal", json!({"changed":[]})).unwrap()["snapshot"].clone();
        fs::write(f.root.join("a"), b"foreign writer bytes").unwrap();
        let output = f
            .cli()
            .args(["hook", "turn-end", "--changed", "a"])
            .env("FURROW_FAULT", boundary)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{boundary}");
        f.request("sync", json!({})).unwrap();
        let frontier: Value =
            serde_json::from_slice(&fs::read(f.data.join("store-v1/group-commit.json")).unwrap())
                .unwrap();
        let pack_len = fs::metadata(f.data.join("store-v1/packs/pack-000001.agp"))
            .unwrap()
            .len();
        assert!(
            frontier["packs"]["pack-000001.agp"].as_u64().unwrap() < pack_len,
            "an uncommitted orphan must not extend the durable frontier"
        );
        fs::write(f.root.join("a"), b"daemon writes after the crash").unwrap();
        let next = f.request("seal", json!({"changed":["a"]})).unwrap()["snapshot"].clone();
        assert_ne!(next, completed);
        daemon.kill().unwrap();
        daemon.wait().unwrap();
        assert_eq!(f.run(&["status"])["head"], next, "{boundary}");
        f.run(&["rewind", completed.as_str().unwrap(), "--yes"]);
        assert_eq!(fs::read(f.root.join("a")).unwrap(), b"first", "{boundary}");
        f.run(&["rewind", next.as_str().unwrap(), "--yes"]);
        assert_eq!(
            fs::read(f.root.join("a")).unwrap(),
            b"daemon writes after the crash",
            "{boundary}"
        );
    }
}

#[test]
fn p3_relative_and_missing_composed_directories_keep_the_same_cli_plan_scope() {
    // P3/S10: candidate paths are absolute and a missing composed directory remains a valid empty attachment.
    let f = Fixture::new();
    let cell = f._temp.path().join("private");
    fs::create_dir(&cell).unwrap();
    fs::write(cell.join("receipt"), b"private receipt").unwrap();
    let output = f
        .cli()
        .current_dir(f._temp.path())
        .args(["capture-plan", "--cell-dir", "private"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let plan: Value = serde_json::from_slice(&output).unwrap();
    for path in plan["paths"].as_array().unwrap() {
        assert!(PathBuf::from(path.as_str().unwrap()).is_absolute());
    }
    f.cli()
        .current_dir(f._temp.path())
        .args([
            "hook",
            "turn-end",
            "--cell-dir",
            "private",
            "--plan",
            plan["token"].as_str().unwrap(),
        ])
        .assert()
        .success();
    let other = Fixture::new();
    other
        .cli()
        .current_dir(other._temp.path())
        .args(["hook", "turn-end", "--cell-dir", "not-created"])
        .assert()
        .success();
}
