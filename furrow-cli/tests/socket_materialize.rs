#![cfg(unix)]

use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::os::unix::net::UnixListener;
use tempfile::TempDir;

#[test]
fn materialize_portable_work_with_a_socket_marker() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let data = temp.path().join("data");
    fs::create_dir_all(source.join(".git")).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::write(source.join("work.txt"), b"portable work\n").unwrap();
    let _socket = UnixListener::bind(source.join(".git/fsmonitor--daemon.ipc")).unwrap();

    let snapshot = Command::cargo_bin("furrow")
        .unwrap()
        .env("FURROW_DATA_DIR", &data)
        .args(["--json", "--repo"])
        .arg(&source)
        .args(["watch", "--no-daemon"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let snapshot: Value = serde_json::from_slice(&snapshot).unwrap();
    Command::cargo_bin("furrow")
        .unwrap()
        .env("FURROW_DATA_DIR", &data)
        .args(["--json", "--repo"])
        .arg(&destination)
        .args([
            "materialize",
            "--head",
            snapshot["snapshot"].as_str().unwrap(),
        ])
        .assert()
        .success();

    assert_eq!(
        fs::read(destination.join("work.txt")).unwrap(),
        b"portable work\n"
    );
    assert!(!destination.join(".git/fsmonitor--daemon.ipc").exists());
    assert!(source.join(".git/fsmonitor--daemon.ipc").exists());
}
