#![cfg(windows)]

use furrow::platform;
use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

#[test]
fn w5_editor_sharing_denial_retries_then_replaces_without_tearing_bytes() {
    // W5: a real handle denying FILE_SHARE_DELETE blocks replacement until
    // the editor releases it. The old bytes survive every failed attempt.
    let temp = tempfile::tempdir().unwrap();
    let old = temp.path().join("editor");
    let new = temp.path().join("replacement");
    fs::write(&old, b"old").unwrap();
    fs::write(&new, b"replacement\r\n\0\xff").unwrap();
    let handle = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&old)
        .unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        drop(handle);
    });
    let start = Instant::now();
    platform::rename(&new, &old).unwrap();
    assert!(start.elapsed() >= Duration::from_millis(100));
    assert!(start.elapsed() < Duration::from_secs(4));
    release.join().unwrap();
    assert_eq!(fs::read(&old).unwrap(), b"replacement\r\n\0\xff");
}

#[test]
fn w5_permanent_sharing_denial_is_bounded_and_leaves_both_files_intact() {
    // W5: a permanently busy editor produces a useful error within the retry
    // budget and cannot destroy either the destination or replacement.
    let temp = tempfile::tempdir().unwrap();
    let old = temp.path().join("old");
    let new = temp.path().join("new");
    fs::write(&old, b"old").unwrap();
    fs::write(&new, b"new").unwrap();
    let handle = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&old)
        .unwrap();
    let start = Instant::now();
    let error = platform::rename(&new, &old).unwrap_err();
    assert!(error.to_string().contains("bounded retries"));
    assert!(start.elapsed() < Duration::from_secs(4));
    drop(handle);
    assert_eq!(fs::read(old).unwrap(), b"old");
    assert_eq!(fs::read(new).unwrap(), b"new");
}

#[test]
fn w5_lock_exclusion_file_flush_and_directory_sync() {
    // W5: two independent handles contend for the same cross-platform lock;
    // releasing the owner admits the waiter. File and directory flush APIs
    // both complete on NTFS, including the documented directory no-op.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("lock");
    let a = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let b = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    fs2::FileExt::lock_exclusive(&a).unwrap();
    assert!(fs2::FileExt::try_lock_exclusive(&b).is_err());
    fs2::FileExt::unlock(&a).unwrap();
    fs2::FileExt::try_lock_exclusive(&b).unwrap();
    fs2::FileExt::unlock(&b).unwrap();
    platform::sync_path(&path).unwrap();
    platform::sync_path(temp.path()).unwrap();
}

#[test]
fn w4_names_windows_refuses_are_identified_before_filesystem_access() {
    // W4: reserved names also apply before extensions, inside directories,
    // with forbidden characters, and with trailing dots/spaces.
    for name in [
        "aux.c",
        "con",
        "a:b",
        "tail.",
        "space ",
        "dir/COM1.txt",
        "LPT¹",
        "dir/a\\b",
        "NUL.txt",
    ] {
        assert!(
            platform::unsupported_name(name.as_bytes()).is_some(),
            "{name}"
        );
    }
    for name in ["auxiliary.c", "console", "dir/normal.txt", "COM10", "café"] {
        assert!(
            platform::unsupported_name(name.as_bytes()).is_none(),
            "{name}"
        );
    }
}

#[test]
fn p1_restore_spares_unchanged_sibling_files_and_directories() {
    // P1/W2: restoring one file must neither report nor touch an unchanged
    // sibling directory or its contents, including their exact NTFS times.
    use furrow::model::{EntryKind, ObjectId, ObjectKind, Snapshot, SnapshotTrigger};
    use furrow::repository::FurrowRepository;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("tree");
    fs::create_dir_all(root.join("src/deep")).unwrap();
    fs::write(root.join("app.txt"), b"original\n").unwrap();
    fs::write(root.join("src/lib.rs"), b"unchanged\r\n\0\xff").unwrap();
    fs::write(root.join("src/deep/nested.rs"), b"nested unchanged\n").unwrap();
    fs::write(root.join("README.md"), b"readme\n").unwrap();
    let (repo, first) = FurrowRepository::attach_and_seal_in(
        &temp.path().join("data"),
        &root,
        None,
        SnapshotTrigger::Manual,
        Default::default(),
    )
    .unwrap();
    // P1: an older snapshot can contain stale listing times at several levels.
    // Give its directory records older times without touching the live tree.
    fn older_directory_times(store: &furrow::store::ObjectStore, root: ObjectId) -> ObjectId {
        let mut entries = Vec::new();
        furrow::tree::for_each_entry(store, &root, |mut entry| {
            if entry.kind == EntryKind::Directory {
                entry.mtime_secs -= 1;
                entry.target = Some(older_directory_times(store, entry.target.unwrap()));
            }
            entries.push(entry);
            Ok(())
        })
        .unwrap();
        furrow::tree::write(store, entries).unwrap()
    }
    let store = furrow::exchange::open_store(&temp.path().join("data")).unwrap();
    let mut snapshot: Snapshot = store.read_struct(&first, ObjectKind::Snapshot).unwrap();
    snapshot.root_tree = older_directory_times(&store, snapshot.root_tree);
    let first = store.put_struct(ObjectKind::Snapshot, &snapshot).unwrap();
    drop(store);
    let paths = [
        "src",
        "src/deep",
        "src/deep/nested.rs",
        "src/lib.rs",
        "README.md",
    ];
    let before: Vec<_> = paths
        .iter()
        .map(|p| fs::metadata(root.join(p)).unwrap().modified().unwrap())
        .collect();
    fs::write(root.join("app.txt"), b"changed\n").unwrap();
    drop(repo);
    let mut repo =
        FurrowRepository::open_composed_in(&temp.path().join("data"), &root, None).unwrap();
    let plan = repo.plan_rewind(&first, &[]).unwrap();
    assert_eq!(plan.changes.len(), 1, "{plan:?}");
    assert_eq!(plan.changes[0].path, "app.txt");
    repo.rewind(&first, &[], false).unwrap();
    assert_eq!(fs::read(root.join("app.txt")).unwrap(), b"original\n");
    assert_eq!(
        fs::read(root.join("src/lib.rs")).unwrap(),
        b"unchanged\r\n\0\xff"
    );
    assert_eq!(fs::read(root.join("README.md")).unwrap(), b"readme\n");
    assert_eq!(
        fs::read(root.join("src/deep/nested.rs")).unwrap(),
        b"nested unchanged\n"
    );
    let after: Vec<_> = paths
        .iter()
        .map(|p| fs::metadata(root.join(p)).unwrap().modified().unwrap())
        .collect();
    assert_eq!(before, after);
}

#[test]
fn p2_native_shell_preserves_quotes_and_failure_status() {
    // P2/W2: quoted paths and failed checks must execute through cmd.exe,
    // rather than echoing a quoted script and falsely reporting success.
    let temp = tempfile::tempdir().unwrap();
    let result = platform::shell(r#"(echo exact)> "file with spaces.txt" & exit /b 17"#)
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(17), "{result:?}");
    assert_eq!(
        fs::read(temp.path().join("file with spaces.txt")).unwrap(),
        b"exact\r\n"
    );
}
