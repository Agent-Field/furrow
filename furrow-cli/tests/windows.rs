#![cfg(windows)]

use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::path::Path;

fn run(data: &Path, root: &Path, args: &[&str]) -> Value {
    let output = Command::cargo_bin("furrow")
        .unwrap()
        .env("FURROW_DATA_DIR", data)
        .env("FURROW_NO_DAEMON", "1")
        .env(
            "FURROW_CELL_KEY",
            "1111111111111111111111111111111111111111111111111111111111111111",
        )
        .env(
            "FURROW_DEDUP_SECRET",
            "2222222222222222222222222222222222222222222222222222222222222222",
        )
        .arg("--json")
        .arg("--repo")
        .arg(root)
        .args(args)
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

#[test]
fn w2_w3_windows_cli_seal_rewind_exchange_rebind_fork_merge_and_long_paths() {
    // W2/W3/W4: exercise the actual binary in isolated NTFS trees, using a
    // path beyond MAX_PATH, mixed line endings, and a non-text byte.
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let root = temp.path().join("tree");
    fs::create_dir(&root).unwrap();
    let long = format!(
        "{}/{}/{}/bytes",
        "a".repeat(95),
        "b".repeat(95),
        "c".repeat(95)
    );
    fs::create_dir_all(root.join(&long).parent().unwrap()).unwrap();
    fs::write(root.join(&long), b"base\r\n\0\xff").unwrap();
    fs::write(root.join("other"), b"keep").unwrap();
    let first = run(&data, &root, &["hook", "turn-end", "--turn", "1"])["snapshot"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(root.join(&long), b"changed\n").unwrap();
    run(
        &data,
        &root,
        &["hook", "turn-end", "--turn", "2", "--changed", &long],
    );
    run(&data, &root, &["snap", "-m", "Windows seal"]);
    fs::write(root.join("other"), b"local").unwrap();
    run(&data, &root, &["rewind", &first, "--yes", "--paths", &long]);
    assert_eq!(fs::read(root.join(&long)).unwrap(), b"base\r\n\0\xff");
    assert_eq!(fs::read(root.join("other")).unwrap(), b"local");
    run(&data, &root, &["rewind", &first, "--yes"]);
    assert_eq!(fs::read(root.join("other")).unwrap(), b"keep");
    let outbox = temp.path().join("outbox");
    let exported = run(
        &data,
        &root,
        &[
            "exchange",
            "export",
            "--head",
            &first,
            "--outbox",
            outbox.to_str().unwrap(),
            "--ledger",
            "0123456789abcdef",
        ],
    );
    let receiver_data = temp.path().join("receiver-data");
    let receiver = temp.path().join("receiver");
    let inbox = temp.path().join("inbox");
    fs::create_dir(&receiver).unwrap();
    fs::create_dir(&inbox).unwrap();
    let frames = exported["frames"].as_array().unwrap();
    for info in frames {
        let bytes = fs::read(info["path"].as_str().unwrap()).unwrap();
        let (_, objects) = furrow::exchange::frame::decode(&bytes).unwrap();
        for object in objects {
            fs::write(inbox.join(furrow::model::id_hex(&object.rid)), object.bytes).unwrap();
        }
    }
    for _ in 0..30 {
        run(
            &receiver_data,
            &receiver,
            &[
                "exchange",
                "import",
                "--head",
                &first,
                "--inbox",
                inbox.to_str().unwrap(),
                "--ledger",
                "0123456789abcdef",
                "--primed",
            ],
        );
        if run(
            &receiver_data,
            &receiver,
            &["exchange", "want", "--head", &first],
        )["want"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            break;
        }
    }
    assert!(run(
        &receiver_data,
        &receiver,
        &["exchange", "want", "--head", &first]
    )["want"]
        .as_array()
        .unwrap()
        .is_empty());
    run(
        &receiver_data,
        &receiver,
        &["exchange", "materialize", "--head", &first],
    );
    assert_eq!(fs::read(receiver.join(&long)).unwrap(), b"base\r\n\0\xff");
    let mut args = vec!["exchange", "published", "--ledger", "0123456789abcdef"];
    for info in frames {
        args.push(info["path"].as_str().unwrap());
    }
    run(&data, &root, &args);
    let moved = temp.path().join("moved");
    fs::rename(&receiver, &moved).unwrap();
    run(
        &receiver_data,
        &moved,
        &["exchange", "rebind", "--from", receiver.to_str().unwrap()],
    );
    let fork = temp.path().join("fork");
    run(
        &data,
        &root,
        &["fork", "work", "--destination", fork.to_str().unwrap()],
    );
    fs::write(fork.join("other"), b"from fork\r\n").unwrap();
    run(&data, &fork, &["snap", "-m", "fork edit"]);
    run(
        &data,
        &root,
        &[
            "merge",
            "work",
            "--check",
            "if exist other (exit /b 0) else (exit /b 1)",
        ],
    );
    assert_eq!(fs::read(root.join("other")).unwrap(), b"from fork\r\n");
}

#[test]
fn w2_serve_cli_idle_zero_exits_cleanly_and_removes_the_socket() {
    // W2: serve's public socket/idle flags work through furrow.exe itself.
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("f.sock");
    Command::cargo_bin("furrow")
        .unwrap()
        .args(["serve", "--socket"])
        .arg(&socket)
        .args(["--idle-minutes", "0"])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success();
    assert!(!socket.exists());
}

#[test]
fn r0_windows_debug_cli_parses_and_captures_a_deep_ten_thousand_entry_tree() {
    // R0/W1: the native executable must parse on Windows' main thread and
    // capture 10,000 files across 64 directory levels without stack overflow.
    Command::cargo_bin("furrow")
        .unwrap()
        .arg("--version")
        .assert()
        .success();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("tree");
    let data = temp.path().join("data");
    fs::create_dir(&root).unwrap();
    let mut directory = root.clone();
    let mut written = 0;
    for depth in 0..64 {
        directory.push("d");
        fs::create_dir(&directory).unwrap();
        let count = if depth < 16 { 157 } else { 156 };
        for number in 0..count {
            fs::write(directory.join(format!("f{number:03}")), b"deep\r\n\0\xff").unwrap();
            written += 1;
        }
    }
    assert_eq!(written, 10_000);
    let start = std::time::Instant::now();
    let answer = run(&data, &root, &["hook", "turn-end", "--turn", "deep"]);
    let store = furrow::exchange::open_store(&data).unwrap();
    let head = furrow::model::parse_id(answer["snapshot"].as_str().unwrap()).unwrap();
    let snapshot: furrow::model::Snapshot = store
        .read_struct(&head, furrow::model::ObjectKind::Snapshot)
        .unwrap();
    let mut pending = vec![snapshot.root_tree];
    let mut files = 0;
    while let Some(tree) = pending.pop() {
        furrow::tree::for_each_entry(&store, &tree, |entry| {
            match entry.kind {
                furrow::model::EntryKind::Directory => pending.push(entry.target.unwrap()),
                furrow::model::EntryKind::File => files += 1,
                _ => {}
            }
            Ok(())
        })
        .unwrap();
    }
    assert_eq!(files, 10_000);
    eprintln!(
        "R0: depth 64, {files} files captured in {:?}",
        start.elapsed()
    );
}

#[test]
fn p1_cli_restore_spares_unchanged_sibling() {
    // P1/W2: separate CLI invocations must preview only the edited file and
    // preserve unchanged sibling bytes and exact last-write times on NTFS.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("tree");
    std::fs::create_dir_all(root.join("src/deep")).unwrap();
    std::fs::write(root.join("app.txt"), b"original\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), b"untouched\r\n\0\xff").unwrap();
    std::fs::write(root.join("README.md"), b"readme\n").unwrap();
    std::fs::write(root.join("src/deep/nested.rs"), b"nested unchanged\n").unwrap();
    let run = |args: &[&str]| {
        let out = std::process::Command::new(
            std::env::var_os("CARGO_BIN_EXE_furrow")
                .unwrap_or_else(|| env!("CARGO_BIN_EXE_furrow").into()),
        )
        .env("FURROW_DATA_DIR", temp.path().join("data"))
        .env("FURROW_NO_DAEMON", "1")
        .env("FURROW_P1_DIAG", "1")
        .arg("--repo")
        .arg(&root)
        .arg("--json")
        .args(args)
        .output()
        .unwrap();
        assert!(out.status.success(), "{out:?}");
        out
    };
    let initial = run(&["watch", "--no-daemon"]);
    let head = serde_json::from_slice::<serde_json::Value>(&initial.stdout).unwrap()["snapshot"]
        .as_str()
        .unwrap()
        .to_owned();
    let times = || {
        [
            "src",
            "src/deep",
            "src/deep/nested.rs",
            "src/lib.rs",
            "README.md",
        ]
        .map(|p| std::fs::metadata(root.join(p)).unwrap().modified().unwrap())
    };
    let before = times();
    std::fs::write(root.join("app.txt"), b"edited\n").unwrap();
    let preview = run(&["rewind", &head, "--dry-run"]);
    let plan: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(
        plan["changes"].as_array().unwrap().len(),
        1,
        "{plan}: {}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert_eq!(plan["changes"][0]["path"], "app.txt");
    run(&["rewind", &head, "--yes"]);
    assert_eq!(times(), before);
    assert_eq!(
        std::fs::read(root.join("src/deep/nested.rs")).unwrap(),
        b"nested unchanged\n"
    );
    assert_eq!(
        std::fs::read(root.join("src/lib.rs")).unwrap(),
        b"untouched\r\n\0\xff"
    );
    assert_eq!(std::fs::read(root.join("README.md")).unwrap(), b"readme\n");
    assert_eq!(std::fs::read(root.join("app.txt")).unwrap(), b"original\n");
}
