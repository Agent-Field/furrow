use super::*;

#[test]
fn w3_windows_keeps_recorded_permissions_with_or_without_file_type_bits() {
    // W3: the entry kind determines whether a recorded mode belongs to this
    // file. Permission-only modes are retained exactly, including executable
    // bits, just as full Unix stat modes are retained.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("tree");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("tool"), b"unchanged\r\n\0\xff").unwrap();
    let (mut repository, original) = FurrowRepository::attach_and_seal_in(
        &temp.path().join("data"),
        &root,
        None,
        SnapshotTrigger::Manual,
        SealOptions::default(),
    )
    .unwrap();
    for mode in [0o755, 0o100755] {
        let mut incoming: Snapshot = repository
            .store
            .read_struct(&original, ObjectKind::Snapshot)
            .unwrap();
        let mut tool = tree::find_path(&repository.store, &incoming.root_tree, b"tool")
            .unwrap()
            .unwrap();
        tool.mode = mode;
        incoming.root_tree = tree::write(&repository.store, vec![tool]).unwrap();
        let incoming = repository
            .store
            .put_struct(ObjectKind::Snapshot, &incoming)
            .unwrap();
        repository.adopt_head(&incoming).unwrap();
        let sealed = repository.snapshot(None, SnapshotTrigger::Manual).unwrap();
        let sealed: Snapshot = repository
            .store
            .read_struct(&sealed, ObjectKind::Snapshot)
            .unwrap();
        let tool = tree::find_path(&repository.store, &sealed.root_tree, b"tool")
            .unwrap()
            .unwrap();
        assert_eq!(tool.mode, mode);
        assert_eq!(fs::read(root.join("tool")).unwrap(), b"unchanged\r\n\0\xff");
    }
}
