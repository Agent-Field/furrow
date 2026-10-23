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

thread_local! {
    pub(super) static CONTENT_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[test]
fn r1_second_full_capture_of_a_thousand_settled_files_reads_no_contents() {
    // R1: an unchanged settled tree uses directory-listing metadata and the
    // stat cache, including across reopening the repository, with zero reads
    // through the engine's content path. A later write must still be captured.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("tree");
    let data = temp.path().join("data");
    fs::create_dir(&root).unwrap();
    let old = filetime::FileTime::from_unix_time(1_700_000_000, 123_456_700);
    for number in 0..1000 {
        let path = root.join(format!("file-{number:04}"));
        fs::write(&path, b"original\r\n\0\xff").unwrap();
        filetime::set_file_mtime(&path, old).unwrap();
    }
    CONTENT_READS.with(|count| count.set(0));
    let (repository, _) = FurrowRepository::attach_and_seal_in(
        &data,
        &root,
        None,
        SnapshotTrigger::Manual,
        SealOptions::default(),
    )
    .unwrap();
    assert_eq!(CONTENT_READS.with(|count| count.get()), 1000);
    drop(repository);
    let mut repository = FurrowRepository::open_composed_in(&data, &root, None).unwrap();
    CONTENT_READS.with(|count| count.set(0));
    let start = Instant::now();
    repository.snapshot(None, SnapshotTrigger::Manual).unwrap();
    let reads = CONTENT_READS.with(|count| count.get());
    eprintln!(
        "R1: second 1000-file full capture: {reads} content opens, {:?}",
        start.elapsed()
    );
    assert_eq!(reads, 0);
    fs::write(root.join("file-0000"), b"modified\r\n\0\xff").unwrap();
    let head = repository.snapshot(None, SnapshotTrigger::Manual).unwrap();
    assert_eq!(CONTENT_READS.with(|count| count.get()), 1);
    let snap: Snapshot = repository
        .store
        .read_struct(&head, ObjectKind::Snapshot)
        .unwrap();
    let entry = tree::find_path(&repository.store, &snap.root_tree, b"file-0000")
        .unwrap()
        .unwrap();
    let blob: Blob = repository
        .store
        .read_struct(&entry.target.unwrap(), ObjectKind::Blob)
        .unwrap();
    let bytes: Vec<u8> = blob
        .chunks
        .iter()
        .flat_map(|chunk| {
            repository
                .store
                .read_bytes(&chunk.id, ObjectKind::Chunk)
                .unwrap()
        })
        .collect();
    assert_eq!(bytes, b"modified\r\n\0\xff");
}

#[test]
fn r1_windows_cache_checks_time_size_kind_and_attributes_but_not_identity() {
    // R1: Windows trusts exact 100 ns last-write time, size, kind and relevant
    // attributes, while ignoring Unix identity and creation time. The racy
    // window still forces content reads when matching metadata is ambiguous.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file");
    fs::write(&path, b"first").unwrap();
    filetime::set_file_mtime(
        &path,
        filetime::FileTime::from_unix_time(1_700_000_000, 100),
    )
    .unwrap();
    let metadata = fs::symlink_metadata(&path).unwrap();
    let mut row = cached_from_read(&FileRead {
        blob: [0; 32],
        xattrs: None,
        sqlite: false,
        stat: metadata.clone(),
        read_at_nanos: 1_700_000_010_000_000_000,
    });
    row.device = 111;
    row.inode = 222;
    row.ctime_secs = i64::MAX;
    assert!(cached_matches(&row, &metadata));
    assert!(!row.is_racy());
    row.cached_at_nanos = 1_700_000_000_000_000_100;
    assert!(row.is_racy());
    for mask in [0o100000, 1 << 20, 0x400 << 20] {
        let mut changed = row.clone();
        changed.mode ^= mask;
        assert!(!cached_matches(&changed, &metadata));
    }
    let mut changed = row.clone();
    changed.size += 1;
    assert!(!cached_matches(&changed, &metadata));
    filetime::set_file_mtime(
        &path,
        filetime::FileTime::from_unix_time(1_700_000_000, 200),
    )
    .unwrap();
    assert!(!cached_matches(&row, &fs::symlink_metadata(&path).unwrap()));
    filetime::set_file_mtime(
        &path,
        filetime::FileTime::from_unix_time(1_700_000_000, 100),
    )
    .unwrap();
    assert!(cached_matches(&row, &fs::symlink_metadata(&path).unwrap()));
    let original_permissions = fs::metadata(&path).unwrap().permissions();
    let mut permissions = original_permissions.clone();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions).unwrap();
    assert!(!cached_matches(&row, &fs::symlink_metadata(&path).unwrap()));
    // Restore the writable attribute so the temporary directory can be removed.
    fs::set_permissions(&path, original_permissions).unwrap();
}
