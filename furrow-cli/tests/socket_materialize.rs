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

    let plan = || -> Value {
        let output = Command::cargo_bin("furrow")
            .unwrap()
            .env("FURROW_DATA_DIR", &data)
            .args(["--json", "--repo"])
            .arg(&destination)
            .args([
                "rewind",
                snapshot["snapshot"].as_str().unwrap(),
                "--dry-run",
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&output).unwrap()
    };
    assert!(plan()["changes"].as_array().unwrap().is_empty());
    for args in [
        vec!["watch", "--no-daemon"],
        vec!["snap", "-m", "unchanged destination"],
    ] {
        Command::cargo_bin("furrow")
            .unwrap()
            .env("FURROW_DATA_DIR", &data)
            .args(["--json", "--repo"])
            .arg(&destination)
            .args(args)
            .assert()
            .success();
        assert!(plan()["changes"].as_array().unwrap().is_empty());
    }

    fs::write(destination.join("work.txt"), b"local edit\n").unwrap();
    assert!(plan()["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["path"] == "work.txt"));
    let marker = destination.join(".git/fsmonitor--daemon.ipc");
    fs::write(&marker, b"local notes\n").unwrap();
    assert!(plan()["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["path"] == ".git/fsmonitor--daemon.ipc"));
    assert_eq!(fs::read(&marker).unwrap(), b"local notes\n");
    fs::remove_file(&marker).unwrap();
    std::os::unix::fs::symlink("../work.txt", &marker).unwrap();
    assert!(plan()["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["path"] == ".git/fsmonitor--daemon.ipc"));
    assert_eq!(
        fs::read_link(&marker).unwrap(),
        std::path::Path::new("../work.txt")
    );
}
