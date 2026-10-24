//! W3/W4: a Linux-created encrypted exchange fixture is consumed unchanged on
//! both platforms. The Windows runner can emit its new seal for Linux to read.

use furrow::exchange::{frame, open_store, ops};
use furrow::model::{id_hex, parse_id, ObjectKind, Snapshot, SnapshotTrigger};
use furrow::repository::FurrowRepository;
#[cfg(unix)]
use furrow::repository::SealOptions;
use furrow::sealer::Keys;
use std::fs;
use std::path::Path;

const LEDGER: &str = "0123456789abcdef";
const CONTENT: &[u8] = b"#!/bin/sh\nline one\r\nline two\n\0\xff";
fn keys() -> Keys {
    Keys {
        cell_key: [0x11; 32],
        dedup: [0x22; 32],
    }
}

fn ingest(data: &Path, inbox: &Path, bytes: &[u8], head: &str) {
    fs::create_dir_all(inbox).unwrap();
    let (_, objects) = frame::decode(bytes).unwrap();
    for object in objects {
        fs::write(inbox.join(id_hex(&object.rid)), object.bytes).unwrap();
    }
    for _ in 0..30 {
        ops::import(data, &keys(), head, LEDGER, inbox, ops::ImportMode::Primed).unwrap();
        if ops::wanted(data, &keys(), head).unwrap()["want"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            return;
        }
    }
    panic!("fixture did not finish importing");
}

// W3: generate the sender fixture using actual Unix capture and export, not
// a second implementation of the object or encrypted exchange format.
#[cfg(unix)]
fn generate_fixture(destination: &Path) {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("tree");
    let data = temp.path().join("data");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("nested/tool"), CONTENT).unwrap();
    fs::hard_link(root.join("nested/tool"), root.join("tool-alias")).unwrap();
    fs::set_permissions(root.join("nested/tool"), fs::Permissions::from_mode(0o755)).unwrap();
    for name in ["aux.c", "con", "a:b", "tail.", "space ", "README", "readme"] {
        fs::write(root.join(name), name.as_bytes()).unwrap();
    }
    symlink("nested/tool", root.join("link")).unwrap();
    let (_, head) = FurrowRepository::attach_and_seal_in(
        &data,
        &root,
        None,
        SnapshotTrigger::Manual,
        SealOptions::default(),
    )
    .unwrap();
    let report = ops::export(
        &data,
        &keys(),
        &id_hex(&head),
        LEDGER,
        &temp.path().join("out"),
        16 << 20,
    )
    .unwrap();
    let paths = report["frames"].as_array().unwrap();
    assert_eq!(paths.len(), 1);
    fs::create_dir_all(destination).unwrap();
    fs::copy(
        paths[0]["path"].as_str().unwrap(),
        destination.join("linux.agef"),
    )
    .unwrap();
    fs::write(destination.join("linux-head.txt"), id_hex(&head)).unwrap();
}

#[test]
fn w3_linux_exchange_preserves_bytes_paths_and_recorded_modes() {
    // M2/W3: hard-link groups, bytes and modes survive Linux import, native Windows
    // materialize and re-seal, and the Windows export consumed again on Linux.
    #[cfg(unix)]
    if let Some(destination) = std::env::var_os("FURROW_GENERATE_PORTABLE") {
        generate_fixture(Path::new(&destination));
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let root = temp.path().join("tree");
    fs::create_dir(&root).unwrap();
    let external = std::env::var_os("FURROW_PORTABLE_IN");
    let (bytes, head) = match external {
        Some(directory) => (
            fs::read(Path::new(&directory).join("windows.agef")).unwrap(),
            fs::read_to_string(Path::new(&directory).join("windows-head.txt")).unwrap(),
        ),
        None => (
            include_bytes!("../../testdata/windows/linux.agef").to_vec(),
            include_str!("../../testdata/windows/linux-head.txt").to_owned(),
        ),
    };
    ingest(&data, &temp.path().join("inbox"), &bytes, head.trim());
    let answer = ops::restore_head(&data, &root, None, head.trim()).unwrap();
    assert_eq!(fs::read(root.join("nested/tool")).unwrap(), CONTENT);
    assert_linked(&root);
    #[cfg(windows)]
    {
        // W4: each impossible entry is individually reported, while the
        // deterministic first case sibling and all representable files land.
        let held = answer["held"].as_array().unwrap();
        for name in ["aux.c", "con", "a:b", "tail.", "space ", "readme"] {
            assert!(
                held.iter()
                    .any(|entry| entry["path"] == name
                        && !entry["reason"].as_str().unwrap().is_empty()),
                "missing report for {name}: {answer}"
            );
        }
        assert_eq!(fs::read(root.join("README")).unwrap(), b"README");
        assert!(
            root.join("link").symlink_metadata().is_ok()
                || held.iter().any(|entry| entry["path"] == "link"
                    && entry["reason"].as_str().unwrap().contains("symlink"))
        );
    }
    #[cfg(unix)]
    {
        // A file system that folds case (a Mac's) cannot hold README and readme
        // together: the first sibling lands and the other is reported, as on
        // Windows. Everything else in the fixture is representable there.
        if keeps_case_apart(temp.path()) {
            assert!(answer.get("held").is_none(), "{answer}");
        } else {
            let held = answer["held"].as_array().unwrap();
            assert_eq!(held.len(), 1, "{answer}");
            assert_eq!(held[0]["path"], "readme", "{answer}");
            assert_eq!(fs::read(root.join("README")).unwrap(), b"README");
        }
    }
    fs::write(root.join("new"), b"new\r\n").unwrap();
    let mut repository = FurrowRepository::open_composed_in(&data, &root, None).unwrap();
    let new_head = repository.snapshot(None, SnapshotTrigger::Manual).unwrap();
    drop(repository);
    let store = open_store(&data).unwrap();
    let snapshot: Snapshot = store.read_struct(&new_head, ObjectKind::Snapshot).unwrap();
    assert_eq!(snapshot.hardlinks.len(), 1);
    assert_eq!(snapshot.hardlinks[0].paths.len(), 2);
    let tool = furrow::tree::find_path(&store, &snapshot.root_tree, b"nested/tool")
        .unwrap()
        .unwrap();
    let new = furrow::tree::find_path(&store, &snapshot.root_tree, b"new")
        .unwrap()
        .unwrap();
    assert_eq!(tool.mode & 0o777, 0o755);
    assert_eq!(new.mode & 0o777, 0o644);
    assert!(
        furrow::tree::find_path(&store, &snapshot.root_tree, b"nested\\tool")
            .unwrap()
            .is_none()
    );
    drop(store);
    let out = temp.path().join("out");
    let exported = ops::export(
        &data,
        &keys(),
        &id_hex(&new_head),
        "fedcba9876543210",
        &out,
        16 << 20,
    )
    .unwrap();
    let frame_path = Path::new(exported["frames"][0]["path"].as_str().unwrap());
    let second_data = temp.path().join("second-data");
    ingest(
        &second_data,
        &temp.path().join("second-inbox"),
        &fs::read(frame_path).unwrap(),
        &id_hex(&new_head),
    );
    let second_tree = temp.path().join("second-tree");
    fs::create_dir(&second_tree).unwrap();
    ops::restore_head(&second_data, &second_tree, None, &id_hex(&new_head)).unwrap();
    assert_eq!(fs::read(second_tree.join("nested/tool")).unwrap(), CONTENT);
    assert_linked(&second_tree);
    assert_eq!(fs::read(second_tree.join("new")).unwrap(), b"new\r\n");
    assert!(
        ops::wanted(&second_data, &keys(), &id_hex(&new_head)).unwrap()["want"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    #[cfg(windows)]
    if let Some(destination) = std::env::var_os("FURROW_PORTABLE_OUT") {
        let destination = Path::new(&destination);
        fs::create_dir_all(destination).unwrap();
        fs::copy(frame_path, destination.join("windows.agef")).unwrap();
        fs::write(destination.join("windows-head.txt"), id_hex(&new_head)).unwrap();
    }
    let paths = exported["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap().into())
        .collect::<Vec<_>>();
    assert!(
        ops::record_published(&data, "fedcba9876543210", &paths).unwrap()["recorded"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(paths.iter().all(|path| !path.exists()));
    assert!(parse_id(head.trim()).is_ok());
}

fn assert_linked(root: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let a = fs::metadata(root.join("nested/tool")).unwrap();
        let b = fs::metadata(root.join("tool-alias")).unwrap();
        assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()));
    }
    #[cfg(windows)]
    {
        let a = furrow::platform::path_identity(&root.join("nested/tool"))
            .unwrap()
            .unwrap();
        let b = furrow::platform::path_identity(&root.join("tool-alias"))
            .unwrap()
            .unwrap();
        assert_eq!(a, b);
    }
}

/// Whether the file system under `dir` keeps names that differ only in case apart.
#[cfg(unix)]
fn keeps_case_apart(dir: &Path) -> bool {
    let probe = dir.join("case-probe");
    fs::create_dir(&probe).unwrap();
    fs::write(probe.join("A"), b"").unwrap();
    fs::write(probe.join("a"), b"").unwrap();
    let apart = fs::read_dir(&probe).unwrap().count() == 2;
    fs::remove_dir_all(&probe).unwrap();
    apart
}
