#![cfg(unix)]
use std::{fs, process::Command};
fn call(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_furrow"))
        .args(args)
        .output()
        .unwrap()
}
#[cfg(target_os = "linux")]
#[test]
fn archive_reopens_large_indexed_directories() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::create_dir(&source).unwrap();
    for i in 0..4096 {
        fs::write(source.join(format!("entry-{i:04}")), b"contents").unwrap();
    }
    let store = t.path().join("store");
    let destination = t.path().join("restored");
    let captured = call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap(),
    ]);
    assert!(
        captured.status.success(),
        "{}",
        String::from_utf8_lossy(&captured.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&captured.stdout).unwrap();
    let snapshot = receipt["snapshot"].as_str().unwrap();
    let restored = call(&[
        "archive",
        "restore",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        snapshot,
        "--destination",
        destination.to_str().unwrap(),
    ]);
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 4096);
    for i in 0..4096 {
        assert_eq!(
            fs::read(destination.join(format!("entry-{i:04}"))).unwrap(),
            b"contents"
        );
    }
    let verified = call(&[
        "archive",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        snapshot,
        "--workspace",
        destination.to_str().unwrap(),
    ]);
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
}
#[test]
fn archive_reopens_exact_files_without_touching_original() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("untracked"), b"original bytes").unwrap();
    let store = t.path().join("store");
    let dest = t.path().join("restored");
    let captured = call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap(),
    ]);
    assert!(
        captured.status.success(),
        "{}",
        String::from_utf8_lossy(&captured.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&captured.stdout).unwrap();
    let id = value["snapshot"].as_str().unwrap();
    let restored = call(&[
        "archive",
        "restore",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        id,
        "--destination",
        dest.to_str().unwrap(),
    ]);
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(fs::read(dest.join("untracked")).unwrap(), b"original bytes");
    let verified = call(&[
        "archive",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        id,
        "--workspace",
        dest.to_str().unwrap(),
    ]);
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    assert_eq!(
        fs::read(source.join("untracked")).unwrap(),
        b"original bytes"
    );
}
#[cfg(target_os = "linux")]
#[test]
fn archive_rejects_linux_nodump_metadata_without_modifying_files() {
    use std::os::fd::AsRawFd;
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::create_dir(&source).unwrap();
    let path = source.join("file");
    fs::write(&path, b"keep original").unwrap();
    let file = fs::File::open(&path).unwrap();
    let mut flags: libc::c_long = 0;
    if unsafe { libc::ioctl(file.as_raw_fd(), 0x80086601u64, &mut flags) } != 0 {
        return; // This filesystem does not expose Linux inode flags.
    }
    let original = flags;
    flags |= 0x40; // Linux fs.h: FS_NODUMP_FL is meaningful metadata.
    if unsafe { libc::ioctl(file.as_raw_fd(), 0x40086602u64, &flags) } != 0 {
        return; // Changing flags is unavailable on this filesystem.
    }
    let store = t.path().join("store");
    let captured = call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap(),
    ]);
    let mut retained: libc::c_long = 0;
    let inspected = unsafe { libc::ioctl(file.as_raw_fd(), 0x80086601u64, &mut retained) };
    let reset = unsafe { libc::ioctl(file.as_raw_fd(), 0x40086602u64, &original) };
    assert_eq!(reset, 0);
    assert_eq!(inspected, 0);
    assert_eq!(retained, flags);
    assert!(!captured.status.success());
    assert!(String::from_utf8_lossy(&captured.stderr).contains("unsupported filesystem flags"));
    assert_eq!(fs::read(path).unwrap(), b"keep original");
}
#[test]
fn archive_preserves_links_attributes_modes_and_hidden_bytes() {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join(".git")).unwrap();
    fs::write(source.join(".git/HEAD"), b"local git state").unwrap();
    fs::write(source.join("odd\nname"), vec![b'z'; 400000]).unwrap();
    fs::hard_link(source.join("odd\nname"), source.join("second")).unwrap();
    symlink("missing", source.join("dangling")).unwrap();
    xattr::set(source.join("second"), "user.archive-test", b"exact").unwrap();
    fs::set_permissions(source.join("second"), fs::Permissions::from_mode(0o440)).unwrap();
    let store = t.path().join("store");
    let destination = t.path().join("destination");
    let c = call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap(),
    ]);
    assert!(c.status.success(), "{}", String::from_utf8_lossy(&c.stderr));
    let j: serde_json::Value = serde_json::from_slice(&c.stdout).unwrap();
    let id = j["snapshot"].as_str().unwrap();
    let r = call(&[
        "archive",
        "restore",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        id,
        "--destination",
        destination.to_str().unwrap(),
    ]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(
        fs::metadata(destination.join("second")).unwrap().ino(),
        fs::metadata(destination.join("odd\nname")).unwrap().ino()
    );
    assert_eq!(
        xattr::get(destination.join("second"), "user.archive-test")
            .unwrap()
            .unwrap(),
        b"exact"
    );
    assert_eq!(
        fs::read_link(destination.join("dangling")).unwrap(),
        std::path::Path::new("missing")
    );
    assert_eq!(
        fs::metadata(destination.join("second")).unwrap().mode() & 0o777,
        0o440
    );
}
#[test]
fn archive_refuses_corruption_existing_destination_and_changed_workspace() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), b"trusted content").unwrap();
    let store = t.path().join("store");
    let c = call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap(),
    ]);
    assert!(c.status.success());
    let j: serde_json::Value = serde_json::from_slice(&c.stdout).unwrap();
    let id = j["snapshot"].as_str().unwrap();
    let existing = call(&[
        "archive",
        "restore",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        id,
        "--destination",
        source.to_str().unwrap(),
    ]);
    assert!(!existing.status.success());
    fs::write(source.join("file"), b"edited").unwrap();
    let changed = call(&[
        "archive",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        id,
        "--workspace",
        source.to_str().unwrap(),
    ]);
    assert!(!changed.status.success());
    let db = rusqlite::Connection::open(store.join("archive-v1.sqlite")).unwrap();
    db.execute("UPDATE chunks SET bytes=x'00'", []).unwrap();
    let dest = t.path().join("recovered");
    let bad = call(&[
        "archive",
        "restore",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        id,
        "--destination",
        dest.to_str().unwrap(),
    ]);
    assert!(!bad.status.success());
    assert!(!dest.exists());
    assert_eq!(fs::read(source.join("file")).unwrap(), b"edited");
}
#[test]
fn archive_rejects_sparse_and_external_hardlinks_without_modifying_files() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::create_dir(&source).unwrap();
    let f = fs::File::create(source.join("sparse")).unwrap();
    f.set_len(1048576).unwrap();
    let store = t.path().join("store");
    assert!(!call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap()
    ])
    .status
    .success());
    fs::remove_file(source.join("sparse")).unwrap();
    fs::write(source.join("linked"), b"outside owner").unwrap();
    fs::hard_link(source.join("linked"), t.path().join("external")).unwrap();
    assert!(!call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap()
    ])
    .status
    .success());
    assert_eq!(
        fs::read(t.path().join("external")).unwrap(),
        b"outside owner"
    );
}
#[test]
fn archives_share_storage_but_restored_workspaces_are_independent() {
    use std::os::unix::fs::MetadataExt;
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), vec![b'a'; 900000]).unwrap();
    let store = t.path().join("store");
    let capture = || {
        let c = call(&[
            "archive",
            "capture",
            "--store",
            store.to_str().unwrap(),
            "--workspace",
            source.to_str().unwrap(),
        ]);
        assert!(c.status.success());
        serde_json::from_slice::<serde_json::Value>(&c.stdout).unwrap()["snapshot"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let id = capture();
    let before = fs::metadata(store.join("archive-v1.sqlite")).unwrap().len();
    assert_eq!(capture(), id);
    assert_eq!(
        fs::metadata(store.join("archive-v1.sqlite")).unwrap().len(),
        before
    );
    let a = t.path().join("a");
    let b = t.path().join("b");
    for d in [&a, &b] {
        let r = call(&[
            "archive",
            "restore",
            "--store",
            store.to_str().unwrap(),
            "--snapshot",
            &id,
            "--destination",
            d.to_str().unwrap(),
        ]);
        assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    }
    assert_ne!(
        fs::metadata(a.join("file")).unwrap().ino(),
        fs::metadata(b.join("file")).unwrap().ino()
    );
    fs::write(a.join("file"), b"changed").unwrap();
    assert_eq!(fs::read(b.join("file")).unwrap().len(), 900000);
    assert_eq!(fs::read(source.join("file")).unwrap().len(), 900000);
}
#[cfg(target_os = "macos")]
#[test]
fn archive_rejects_native_darwin_acl_and_flags() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::create_dir(&source).unwrap();
    let path = source.join("file");
    fs::write(&path, b"keep original").unwrap();
    let store = t.path().join("store");
    assert!(Command::new("chmod")
        .args(["+a", "everyone allow read"])
        .arg(&path)
        .status()
        .unwrap()
        .success());
    let acl = call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap(),
    ]);
    assert!(!acl.status.success());
    assert!(String::from_utf8_lossy(&acl.stderr).contains("ACL"));
    assert!(Command::new("chmod")
        .arg("-N")
        .arg(&path)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("chflags")
        .arg("nodump")
        .arg(&path)
        .status()
        .unwrap()
        .success());
    let flags = call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap(),
    ]);
    assert!(!flags.status.success());
    assert!(String::from_utf8_lossy(&flags.stderr).contains("flags"));
    assert!(Command::new("chflags")
        .arg("0")
        .arg(&path)
        .status()
        .unwrap()
        .success());
    assert_eq!(fs::read(path).unwrap(), b"keep original");
}
#[cfg(target_os = "macos")]
#[test]
fn archive_restores_symlink_mode_independent_of_process_umask() {
    use std::os::unix::{fs::symlink, process::CommandExt};
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::create_dir(&source).unwrap();
    symlink("missing", source.join("link")).unwrap();
    let store = t.path().join("store");
    let destination = t.path().join("restored");
    let c = call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap(),
    ]);
    assert!(c.status.success());
    let j: serde_json::Value = serde_json::from_slice(&c.stdout).unwrap();
    let id = j["snapshot"].as_str().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_furrow"));
    command.args([
        "archive",
        "restore",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        id,
        "--destination",
        destination.to_str().unwrap(),
    ]);
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let r = command.output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
}
#[test]
fn archive_verifies_partial_retirement_but_refuses_changed_remainders() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a"), b"same").unwrap();
    fs::hard_link(source.join("a"), source.join("b")).unwrap();
    fs::write(source.join("c"), b"keep").unwrap();
    let store = t.path().join("store");
    let c = call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap(),
    ]);
    assert!(c.status.success());
    let j: serde_json::Value = serde_json::from_slice(&c.stdout).unwrap();
    let id = j["snapshot"].as_str().unwrap();
    fs::remove_file(source.join("a")).unwrap();
    let args = [
        "archive",
        "verify-remainder",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        id,
        "--workspace",
        source.to_str().unwrap(),
    ];
    let partial = call(&args);
    assert!(
        partial.status.success(),
        "{}",
        String::from_utf8_lossy(&partial.stderr)
    );
    fs::write(source.join("c"), b"changed").unwrap();
    assert!(!call(&args).status.success());
}
#[cfg(target_os = "macos")]
#[test]
fn archive_requires_original_provenance_and_only_tolerates_new_os_marker() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), b"unchanged").unwrap();
    let store = t.path().join("store");
    let c = call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap(),
    ]);
    assert!(c.status.success());
    let original = serde_json::from_slice::<serde_json::Value>(&c.stdout).unwrap()["snapshot"]
        .as_str()
        .unwrap()
        .to_owned();
    // A fixture declaring an original marker must not use the extra-marker exception.
    let db = rusqlite::Connection::open(store.join("archive-v1.sqlite")).unwrap();
    let bytes: Vec<u8> = db
        .query_row(
            "SELECT manifest FROM snapshots WHERE id=?",
            [&original],
            |r| r.get(0),
        )
        .unwrap();
    let mut manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let key = serde_json::to_value(b"com.apple.provenance".to_vec()).unwrap();
    let attrs = manifest["entries"][0]["attrs"].as_array_mut().unwrap();
    attrs.retain(|entry| entry[0] != key);
    attrs.push(serde_json::json!([key, vec![29u8; 32]]));
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let id = blake3::hash(&bytes).to_hex().to_string();
    db.execute(
        "INSERT INTO snapshots VALUES(?1,?2)",
        rusqlite::params![id, bytes],
    )
    .unwrap();
    xattr::set(&source, "com.apple.provenance", &[1, 2, 3, 4]).unwrap();
    let args = [
        "archive",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &id,
        "--workspace",
        source.to_str().unwrap(),
    ];
    assert!(!call(&args).status.success());
    xattr::remove(&source, "com.apple.provenance").unwrap();
    assert!(!call(&args).status.success());
    xattr::set(&source, "user.unexpected", b"extra").unwrap();
    assert!(!call(&[
        "archive",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &original,
        "--workspace",
        source.to_str().unwrap()
    ])
    .status
    .success());
}

#[test]
fn archive_restores_registered_idle_workspace_without_detaching_its_timeline() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("registered");
    let retired = t.path().join("retired");
    let store = t.path().join("archive");
    let data = t.path().join("native");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("work.txt"), b"retained task bytes").unwrap();
    let native = |verb: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_furrow"))
            .env("FURROW_DATA_DIR", &data)
            .args(["--json", "--repo", source.to_str().unwrap()])
            .args(verb)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let attached = native(&["watch", "--no-daemon"]);
    let before = native(&["status"]);
    assert_eq!(before["watcher_running"], false);
    assert_eq!(before["head"], attached["snapshot"]);
    assert!(before["head"].as_str().is_some());
    let captured = call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap(),
    ]);
    assert!(
        captured.status.success(),
        "{}",
        String::from_utf8_lossy(&captured.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&captured.stdout).unwrap();
    let snapshot = receipt["snapshot"].as_str().unwrap();
    assert!(call(&[
        "archive",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        snapshot,
        "--workspace",
        source.to_str().unwrap()
    ])
    .status
    .success());
    fs::rename(&source, &retired).unwrap();
    let restored = call(&[
        "archive",
        "restore",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        snapshot,
        "--destination",
        source.to_str().unwrap(),
    ]);
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert!(call(&[
        "archive",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        snapshot,
        "--workspace",
        source.to_str().unwrap()
    ])
    .status
    .success());
    assert_eq!(
        fs::read(source.join("work.txt")).unwrap(),
        b"retained task bytes"
    );
    assert_eq!(
        fs::read(retired.join(".furrow/workspace-id")).unwrap(),
        fs::read(source.join(".furrow/workspace-id")).unwrap()
    );
    let after = native(&["status"]);
    assert_eq!(after["head"], before["head"]);
    assert_eq!(after["watcher_running"], false);
    assert!(!attached["snapshot"].is_null());
}

#[test]
fn archive_supports_bare_relative_store_and_destination() {
    let t = tempfile::tempdir().unwrap();
    fs::create_dir(t.path().join("source")).unwrap();
    fs::write(t.path().join("source/file"), b"relative bytes").unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_furrow"))
            .current_dir(t.path())
            .args(args)
            .output()
            .unwrap()
    };
    let captured = run(&[
        "archive",
        "capture",
        "--store",
        "vault",
        "--workspace",
        "source",
    ]);
    assert!(
        captured.status.success(),
        "{}",
        String::from_utf8_lossy(&captured.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&captured.stdout).unwrap();
    let restored = run(&[
        "archive",
        "restore",
        "--store",
        "vault",
        "--snapshot",
        receipt["snapshot"].as_str().unwrap(),
        "--destination",
        "restored",
    ]);
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(
        fs::read(t.path().join("restored/file")).unwrap(),
        b"relative bytes"
    );
}

#[test]
fn archive_rejects_manifest_symlink_ancestors_before_writing_outside() {
    use std::os::unix::ffi::OsStrExt;
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::create_dir_all(source.join("a/existingdir")).unwrap();
    fs::write(source.join("a/existingdir/newfile"), b"must stay inside").unwrap();
    let outside = t.path().join("outside");
    fs::create_dir_all(outside.join("existingdir")).unwrap();
    let store = t.path().join("vault");
    let captured = call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap(),
    ]);
    assert!(captured.status.success());
    let receipt: serde_json::Value = serde_json::from_slice(&captured.stdout).unwrap();
    let db = rusqlite::Connection::open(store.join("archive-v1.sqlite")).unwrap();
    let bytes: Vec<u8> = db
        .query_row(
            "SELECT manifest FROM snapshots WHERE id=?",
            [receipt["snapshot"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    let mut manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let entries = manifest["entries"].as_array_mut().unwrap();
    entries.remove(2); // An immediate parent exists only through the outside symlink.
    entries[1]["kind"] = 2.into();
    entries[1]["link"] = serde_json::to_value(outside.as_os_str().as_bytes()).unwrap();
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let id = blake3::hash(&bytes).to_hex().to_string();
    db.execute(
        "INSERT INTO snapshots VALUES(?1,?2)",
        rusqlite::params![id, bytes],
    )
    .unwrap();
    let destination = t.path().join("restored");
    let output = call(&[
        "archive",
        "restore",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &id,
        "--destination",
        destination.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(
        !outside.join("existingdir/newfile").exists(),
        "restore wrote through a symlink ancestor"
    );
    assert!(!destination.exists());
}

#[test]
fn archive_rejects_oversized_manifest_before_loading_blob() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::create_dir(&source).unwrap();
    let store = t.path().join("vault");
    assert!(call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap()
    ])
    .status
    .success());
    let db = rusqlite::Connection::open(store.join("archive-v1.sqlite")).unwrap();
    let id = "a".repeat(64);
    db.execute(
        "INSERT INTO snapshots VALUES(?1,zeroblob(?2))",
        rusqlite::params![id, 256 * 1024 * 1024 + 1],
    )
    .unwrap();
    let destination = t.path().join("restored");
    let output = call(&[
        "archive",
        "restore",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &id,
        "--destination",
        destination.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("manifest exceeds"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!destination.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn archive_syncs_restored_metadata_and_nested_directories_before_publication() {
    if !Command::new("strace")
        .args(["-o", "/dev/null", "true"])
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("syscall-order proof requires strace and ptrace permission");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    fs::create_dir_all(source.join("nested")).unwrap();
    fs::write(source.join("nested/file"), b"durable bytes").unwrap();
    let store = t.path().join("vault");
    let output = call(&[
        "archive",
        "capture",
        "--store",
        store.to_str().unwrap(),
        "--workspace",
        source.to_str().unwrap(),
    ]);
    assert!(output.status.success());
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let trace = t.path().join("trace");
    let destination = t.path().join("restored");
    let output = Command::new("strace")
        .args(["-yy", "-o"])
        .arg(&trace)
        .args([
            "-e",
            "trace=fsync,utimensat,renameat2",
            env!("CARGO_BIN_EXE_furrow"),
            "archive",
            "restore",
            "--store",
            store.to_str().unwrap(),
            "--snapshot",
            receipt["snapshot"].as_str().unwrap(),
            "--destination",
            destination.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let trace = fs::read_to_string(trace).unwrap();
    let lines: Vec<_> = trace.lines().collect();
    let publish = lines
        .iter()
        .position(|line| line.starts_with("renameat2("))
        .unwrap();
    let metadata = lines
        .iter()
        .rposition(|line| line.starts_with("utimensat("))
        .unwrap();
    let durable = &lines[metadata + 1..publish];
    assert!(
        durable
            .iter()
            .any(|line| line.starts_with("fsync(") && line.contains("/nested/file>")),
        "file metadata not synced before publication: {trace}"
    );
    assert!(
        durable
            .iter()
            .any(|line| line.starts_with("fsync(") && line.contains("/nested>")),
        "nested directory not synced before publication: {trace}"
    );
    assert!(
        durable.iter().any(|line| line.starts_with("fsync(")
            && line.contains("/.furrow-archive-")
            && !line.contains("/nested")),
        "root directory not synced before publication: {trace}"
    );
}
