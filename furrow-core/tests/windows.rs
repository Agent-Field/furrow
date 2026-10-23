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
