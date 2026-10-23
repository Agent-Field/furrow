//! Default-on wave contracts: ordered capture, screened scopes, and folder identity.

use super::*;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        let data = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a"), b"first").unwrap();
        Self {
            _temp: temp,
            root,
            data,
        }
    }

    fn attach(&self, timeline: &str) -> FurrowRepository {
        FurrowRepository::attach_timeline_in(&self.data, &self.root, Some(timeline), None).unwrap()
    }
}

fn seal(repo: &mut FurrowRepository) -> ObjectId {
    repo.seal(None, SnapshotTrigger::AgentRun, None).unwrap()
}

#[test]
fn p1_capture_is_visible_before_sync_and_a_torn_pack_suffix_recovers() {
    // P1/C28: completed writes survive process death; a torn power-loss suffix preserves the durable head.
    for tail in [0, 1, 4, 6, 14, 46, 77, 78, 79] {
        let f = Fixture::new();
        let mut repo = f.attach("chat");
        let first = seal(&mut repo);
        repo.defer_durability().unwrap();
        repo.sync().unwrap();
        let pack = repo.store.root().join("packs/pack-000001.agp");
        let durable_len = fs::metadata(&pack).unwrap().len();
        fs::write(f.root.join("a"), b"second capture").unwrap();
        let second = seal(&mut repo);
        assert_ne!(first, second);
        let status = f.attach("chat").status().unwrap();
        assert_eq!(
            status.head,
            Some(id_hex(&second)),
            "a completed capture is visible without a flush"
        );
        drop(repo);
        let file = fs::OpenOptions::new().write(true).open(&pack).unwrap();
        file.set_len(durable_len + tail).unwrap();
        drop(file);
        let mut reopened = f.attach("chat");
        assert_eq!(
            reopened.status().unwrap().head,
            Some(id_hex(&first)),
            "tail {tail}"
        );
        let third = seal(&mut reopened);
        assert_ne!(third, first);
        reopened.sync().unwrap();
        assert_eq!(
            reopened
                .store
                .read_struct::<Snapshot>(&third, ObjectKind::Snapshot)
                .unwrap()
                .parent,
            Some(first)
        );
    }
}

#[test]
fn p1_complete_but_corrupt_uncommitted_trailer_is_removed() {
    // P1/C28: a torn write may leave the final record full length with a bad trailer.
    let f = Fixture::new();
    let mut repo = f.attach("chat");
    let first = seal(&mut repo);
    repo.defer_durability().unwrap();
    fs::write(f.root.join("a"), b"second").unwrap();
    seal(&mut repo);
    let pack = repo.store.root().join("packs/pack-000001.agp");
    drop(repo);
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pack)
        .unwrap();
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::End(-1)).unwrap();
    file.write_all(b"X").unwrap();
    drop(file);
    assert_eq!(
        f.attach("chat").status().unwrap().head,
        Some(id_hex(&first))
    );
}

#[test]
fn p3_first_delta_plan_reports_full_fallback_and_refuses_new_candidates() {
    // P3/C9: an uninitialized index cannot turn a screened delta into an unscreened walk.
    let f = Fixture::new();
    fs::write(f.root.join("b"), b"also screen").unwrap();
    let mut repo = f.attach("chat");
    let plan = repo.capture_plan(Some(&[PathBuf::from("a")])).unwrap();
    assert!(plan.full);
    assert!(plan
        .paths
        .contains(&f.root.canonicalize().unwrap().join("b")));
    fs::write(f.root.join("unscreened"), b"must not capture").unwrap();
    assert!(repo
        .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .is_err());
    assert_eq!(repo.status().unwrap().head, None);
    let plan = repo.capture_plan(None).unwrap();
    repo.seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .unwrap();
}

#[test]
fn p3_delta_plan_is_scoped_and_rejects_changed_identity_or_head() {
    // P3/C9: a token covers the screened candidates, not later file bytes or a different timeline head.
    let f = Fixture::new();
    fs::write(f.root.join("b"), b"unrelated").unwrap();
    let mut repo = f.attach("chat");
    seal(&mut repo);
    let plan = repo.capture_plan(Some(&[PathBuf::from("a")])).unwrap();
    assert!(!plan.full);
    assert_eq!(plan.paths, [f.root.canonicalize().unwrap().join("a")]);
    fs::write(f.root.join("a"), b"changed after screening").unwrap();
    assert!(repo
        .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .is_err());
    let plan = repo.capture_plan(Some(&[PathBuf::from("a")])).unwrap();
    repo.seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .unwrap();
    assert!(repo
        .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .is_err());
    let plan = repo.capture_plan(None).unwrap();
    let mut other = f.attach("other");
    assert!(other
        .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .is_err());
}

#[test]
fn p3_policy_change_and_new_sqlite_sidecar_require_replanning() {
    // P3/C9: policy edits and backup sidecars cannot expand the screened read set.
    let f = Fixture::new();
    let db = rusqlite::Connection::open(f.root.join("db")).unwrap();
    db.execute_batch("CREATE TABLE records (n INTEGER); INSERT INTO records VALUES(1)")
        .unwrap();
    drop(db);
    let mut repo = f.attach("chat");
    seal(&mut repo);
    let plan = repo.capture_plan(Some(&[PathBuf::from("a")])).unwrap();
    assert!(plan.paths.iter().any(|p| p.ends_with("db")));
    fs::write(f.root.join("db-wal"), b"new sidecar").unwrap();
    assert!(repo
        .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .is_err());
    let plan = repo.capture_plan(None).unwrap();
    fs::write(f.root.join(".furrowpolicy"), b"exclude a\n").unwrap();
    assert!(repo
        .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .is_err());
}

#[test]
fn p4_second_timeline_reuses_folder_rows_but_not_composed_private_rows() {
    // P4: the same physical file has one stat identity across chats; private overlays remain distinct.
    let f = Fixture::new();
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let mut first = f.attach("first");
    seal(&mut first);
    let old = first
        .store
        .cached_file(&first.workspace_id, b"a")
        .unwrap()
        .unwrap();
    assert!(!old.is_racy());
    let mut second = f.attach("second");
    seal(&mut second);
    let reused = second
        .store
        .cached_file(&second.workspace_id, b"a")
        .unwrap()
        .unwrap();
    assert_eq!(
        old.cached_at_nanos, reused.cached_at_nanos,
        "second chat must not re-hash unchanged bytes"
    );
    let left = f._temp.path().join("left");
    let right = f._temp.path().join("right");
    fs::create_dir_all(&left).unwrap();
    fs::create_dir_all(&right).unwrap();
    fs::write(left.join("private"), b"left private bytes").unwrap();
    fs::write(right.join("private"), b"right private bytes").unwrap();
    let mut left_repo =
        FurrowRepository::attach_timeline_in(&f.data, &f.root, Some("left"), Some(left)).unwrap();
    let mut right_repo =
        FurrowRepository::attach_timeline_in(&f.data, &f.root, Some("right"), Some(right)).unwrap();
    seal(&mut left_repo);
    let left_row = left_repo
        .store
        .cached_file(&left_repo.workspace_id, b".cell/private")
        .unwrap()
        .unwrap();
    seal(&mut right_repo);
    let right_row = right_repo
        .store
        .cached_file(&right_repo.workspace_id, b".cell/private")
        .unwrap()
        .unwrap();
    assert_ne!(left_row.blob_id, right_row.blob_id);
    assert_eq!(
        left_repo
            .store
            .cached_file(&left_repo.workspace_id, b".cell/private")
            .unwrap()
            .unwrap()
            .blob_id,
        left_row.blob_id
    );

    assert_ne!(
        first.store.workspace_head(&first.workspace_id).unwrap(),
        second.store.workspace_head(&second.workspace_id).unwrap()
    );
}

#[test]
fn p1_p3_a_stale_advisory_index_cannot_drop_a_completed_capture() {
    // P1/C28/P3: a lost advisory index forces a screened full plan and preserves the authoritative head.
    let f = Fixture::new();
    let mut repo = f.attach("chat");
    seal(&mut repo);
    let saved = f._temp.path().join("old-index.sqlite3");
    repo.open_path_index().unwrap().backup_to(&saved).unwrap();
    fs::write(f.root.join("a"), b"second capture").unwrap();
    let second = seal(&mut repo);
    repo.sync().unwrap();
    repo.path_index.borrow_mut().take();
    let index = repo.workspace_data_dir().join("paths.sqlite3");
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(PathBuf::from(format!("{}{suffix}", index.display())));
    }
    fs::copy(saved, index).unwrap();
    fs::write(f.root.join("b"), b"next delta").unwrap();
    let plan = repo.capture_plan(Some(&[PathBuf::from("b")])).unwrap();
    assert!(plan.full, "the guard must screen the full fallback");
    let third = repo
        .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .unwrap();
    let prior: Snapshot = repo
        .store
        .read_struct(&second, ObjectKind::Snapshot)
        .unwrap();
    let next: Snapshot = repo
        .store
        .read_struct(&third, ObjectKind::Snapshot)
        .unwrap();
    assert_eq!(
        repo.lookup_tree_path(&prior.root_tree, b"a").unwrap(),
        repo.lookup_tree_path(&next.root_tree, b"a").unwrap()
    );
}

#[test]
fn p1_p3_screened_database_capture_retains_a_consistent_image() {
    // P1/P3/C9: backup opens operate on an isolated copy of screened database bytes and retain logical contents.
    let f = Fixture::new();
    let database = rusqlite::Connection::open(f.root.join("db")).unwrap();
    database
        .execute_batch("CREATE TABLE records (n INTEGER); INSERT INTO records VALUES(7)")
        .unwrap();
    drop(database);
    let mut repo = f.attach("chat");
    let plan = repo.capture_plan(None).unwrap();
    repo.defer_durability().unwrap();
    let id = repo
        .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .unwrap();
    repo.sync().unwrap();
    let snapshot: Snapshot = repo.store.read_struct(&id, ObjectKind::Snapshot).unwrap();
    assert_eq!(snapshot.sqlite_backups.len(), 1);
    assert!(snapshot.sqlite_backups[0].integrity_ok);
    assert_eq!(snapshot.sqlite_backups[0].path, b"db");
}

#[test]
fn p1_power_loss_of_a_new_pack_entry_removes_only_the_uncommitted_group() {
    // P1/C28: the catalog may outlive a newly created pack's unsynced directory entry.
    let f = Fixture::new();
    let mut repo = f.attach("chat");
    repo.defer_durability().unwrap();
    seal(&mut repo);
    let pack = repo.store.root().join("packs/pack-000001.agp");
    drop(repo);
    fs::remove_file(pack).unwrap();
    let mut recovered = f.attach("chat");
    assert_eq!(recovered.status().unwrap().head, None);
    seal(&mut recovered);
    recovered.sync().unwrap();
}

#[test]
fn p4_s10_existing_timeline_rows_are_shared_before_a_new_chats_first_capture() {
    // P4/S10: opening a pre-wave store reuses its stable file identities in another timeline, without re-hashing.
    let f = Fixture::new();
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let mut first = f.attach("old");
    seal(&mut first);
    let row = first
        .store
        .cached_file(&first.workspace_id, b"a")
        .unwrap()
        .unwrap();
    assert!(!row.is_racy());
    let id = first.workspace_id.clone();
    drop(first);
    // This is the old on-disk cache namespace, retained with the same row schema.
    let catalog = crate::catalog::Catalog::open(&f.data.join("store-v1/catalog.sqlite3")).unwrap();
    catalog.cache_file(&id, b"a", &row).unwrap();
    drop(catalog);
    let connection = rusqlite::Connection::open(f.data.join("store-v1/catalog.sqlite3")).unwrap();
    connection
        .execute("DELETE FROM stat_cache WHERE workspace_id != ?1", [&id])
        .unwrap();
    drop(connection);
    let mut second = f.attach("new");
    seal(&mut second);
    let shared = second
        .store
        .cached_file(&second.workspace_id, b"a")
        .unwrap()
        .unwrap();
    assert_eq!(shared.cached_at_nanos, row.cached_at_nanos);
}

#[test]
fn p3_overlay_exclusion_changes_force_a_full_screenable_capture() {
    // P3/C9: a composed policy change affects the whole tree even when the caller names another file.
    let f = Fixture::new();
    let cell = f._temp.path().join("cell");
    fs::create_dir(&cell).unwrap();
    fs::write(f.root.join("other"), b"keep").unwrap();
    let mut repo =
        FurrowRepository::attach_timeline_in(&f.data, &f.root, Some("chat"), Some(cell.clone()))
            .unwrap();
    seal(&mut repo);
    fs::write(cell.join(".furrowpolicy"), b"exclude a\n").unwrap();
    let plan = repo.capture_plan(Some(&[PathBuf::from("other")])).unwrap();
    assert!(plan.full);
    assert!(!plan
        .paths
        .iter()
        .any(|p| p == &f.root.canonicalize().unwrap().join("a")));
    let id = repo
        .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .unwrap();
    let snapshot: Snapshot = repo.store.read_struct(&id, ObjectKind::Snapshot).unwrap();
    assert!(repo
        .lookup_tree_path(&snapshot.root_tree, b"a")
        .unwrap()
        .is_none());
    assert!(repo
        .lookup_tree_path(&snapshot.root_tree, b"other")
        .unwrap()
        .is_some());
}

fn snapshot_bytes(repo: &FurrowRepository, id: &ObjectId, path: &str) -> Option<Vec<u8>> {
    let snap: Snapshot = repo.store.read_struct(id, ObjectKind::Snapshot).unwrap();
    let entry = repo
        .lookup_tree_path(&snap.root_tree, path.as_bytes())
        .unwrap()?;
    let blob: Blob = repo
        .store
        .read_struct(&entry.target?, ObjectKind::Blob)
        .unwrap();
    Some(
        blob.chunks
            .iter()
            .flat_map(|c| repo.store.read_bytes(&c.id, ObjectKind::Chunk).unwrap())
            .collect(),
    )
}

#[test]
fn q3_immediate_writes_plans_and_captures_never_lose_a_change() {
    // Q3/C9: a deterministic property sequence interleaves external writes, removals, renames, plans and captures.
    let f = Fixture::new();
    let mut repo = f.attach("watch");
    repo.defer_durability().unwrap();
    repo.enable_change_tracking();
    seal(&mut repo);
    let mut expected = BTreeMap::from([("a".to_owned(), b"first".to_vec())]);
    let mut random = 0x6f31_27d5_u64;
    for iteration in 0..160 {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let name = format!("p{}", random % 12);
        match random % 5 {
            0 => {
                let _ = fs::remove_file(f.root.join(&name));
                expected.remove(&name);
            }
            1 if expected.contains_key(&name) => {
                let renamed = format!("p{}", (random + 1) % 12);
                fs::rename(f.root.join(&name), f.root.join(&renamed)).unwrap();
                let value = expected.remove(&name).unwrap();
                expected.insert(renamed, value);
            }
            _ => {
                let bytes = format!("value {iteration} {random}").into_bytes();
                fs::write(f.root.join(&name), &bytes).unwrap();
                expected.insert(name, bytes);
            }
        }
        let id = if iteration % 2 == 0 {
            let plan = repo.capture_plan(None).unwrap();
            repo.seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
                .unwrap()
        } else {
            seal(&mut repo)
        };
        for n in std::iter::once("a".to_owned()).chain((0..12).map(|i| format!("p{i}"))) {
            assert_eq!(
                snapshot_bytes(&repo, &id, &n),
                expected.get(&n).cloned(),
                "iteration {iteration}, {n}"
            );
        }
    }
}

#[test]
fn q3_cold_restart_and_guard_scope_cannot_miss_external_edits() {
    // Q3/P3/C9: downtime forces a walk, and a new path after screening refuses a narrowed token.
    let f = Fixture::new();
    let mut repo = f.attach("watch");
    repo.enable_change_tracking();
    seal(&mut repo);
    drop(repo);
    fs::write(f.root.join("a"), b"while down").unwrap();
    let mut repo = f.attach("watch");
    repo.enable_change_tracking();
    assert!(repo.capture_plan(None).unwrap().full);
    let id = seal(&mut repo);
    assert_eq!(
        snapshot_bytes(&repo, &id, "a"),
        Some(b"while down".to_vec())
    );
    let plan = repo.capture_plan(None).unwrap();
    fs::write(f.root.join("outside"), b"needs screening").unwrap();
    if !plan.full {
        assert!(repo
            .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
            .is_err());
    }
    let plan = repo.capture_plan(None).unwrap();
    assert!(
        plan.paths.contains(&repo.root().join("outside")),
        "{:?}",
        plan.paths
    );
    let id = repo
        .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .unwrap();
    assert_eq!(
        snapshot_bytes(&repo, &id, "outside"),
        Some(b"needs screening".to_vec())
    );
}

#[test]
fn q3_known_hints_do_not_clear_unrelated_editor_changes() {
    // Q3/C9: an executor's known hints cannot consume another writer's pending paths without capturing them.
    let f = Fixture::new();
    let mut repo = f.attach("watch");
    repo.enable_change_tracking();
    seal(&mut repo);
    fs::write(f.root.join("outside"), b"editor").unwrap();
    fs::write(f.root.join("a"), b"executor").unwrap();
    let plan = repo.capture_plan(Some(&[PathBuf::from("a")])).unwrap();
    assert!(!plan.full);
    assert_eq!(plan.paths, vec![repo.root().join("a")]);
    let known = repo
        .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .unwrap();
    assert_eq!(snapshot_bytes(&repo, &known, "outside"), None);
    let id = seal(&mut repo);
    assert_eq!(
        snapshot_bytes(&repo, &id, "outside"),
        Some(b"editor".to_vec())
    );
}

#[test]
fn q2_unchanged_full_capture_never_updates_nonracy_stat_rows() {
    // Q2/R1: a full walk may publish a snapshot, but unchanged stable file identities cause no cache writes.
    let f = Fixture::new();
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let mut repo = f.attach("cache");
    seal(&mut repo);
    let catalog = rusqlite::Connection::open(repo.store.root().join("catalog.sqlite3")).unwrap();
    catalog.execute_batch("CREATE TABLE cache_writes (n); INSERT INTO cache_writes VALUES (0);
        CREATE TRIGGER count_cache_writes AFTER UPDATE ON stat_cache BEGIN UPDATE cache_writes SET n=n+1; END;").unwrap();
    seal(&mut repo);
    let writes: i64 = catalog
        .query_row("SELECT n FROM cache_writes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(writes, 0);
}

#[test]
fn q3_directory_topology_and_hardlink_edits_widen_the_plan() {
    // Q3/P3: registration changes and aliases require a full screened walk, preserving every linked name.
    let f = Fixture::new();
    let mut repo = f.attach("links");
    repo.enable_change_tracking();
    seal(&mut repo);
    fs::create_dir(f.root.join("new")).unwrap();
    fs::write(f.root.join("new/a"), b"in new directory").unwrap();
    assert!(repo.capture_plan(None).unwrap().full);
    seal(&mut repo);
    seal(&mut repo);
    fs::hard_link(f.root.join("a"), f.root.join("alias")).unwrap();
    assert!(repo.capture_plan(None).unwrap().full);
    seal(&mut repo);
    fs::write(f.root.join("alias"), b"through alias").unwrap();
    assert!(repo.capture_plan(None).unwrap().full);
    let id = seal(&mut repo);
    assert_eq!(
        snapshot_bytes(&repo, &id, "a"),
        Some(b"through alias".to_vec())
    );
    assert_eq!(
        snapshot_bytes(&repo, &id, "alias"),
        Some(b"through alias".to_vec())
    );
}

#[test]
fn q3_ignore_rule_changes_register_previously_ignored_directories() {
    // Q3/G1: changing exclusion rules must install newly relevant watches before trusting another delta.
    let f = Fixture::new();
    fs::create_dir_all(f.root.join(".git/objects")).unwrap();
    fs::write(f.root.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    fs::write(f.root.join(".furrowpolicy"), b"gitignore\n").unwrap();
    fs::write(f.root.join(".gitignore"), b"hidden/\n").unwrap();
    fs::create_dir(f.root.join("hidden")).unwrap();
    fs::write(f.root.join("hidden/a"), b"was ignored").unwrap();
    let mut repo = f.attach("rules");
    repo.enable_change_tracking();
    seal(&mut repo);
    fs::write(f.root.join(".gitignore"), b"").unwrap();
    assert!(repo.capture_plan(None).unwrap().full);
    seal(&mut repo);
    seal(&mut repo);
    fs::write(f.root.join("hidden/a"), b"now included").unwrap();
    let id = seal(&mut repo);
    assert_eq!(
        snapshot_bytes(&repo, &id, "hidden/a"),
        Some(b"now included".to_vec())
    );
}

#[cfg(target_os = "linux")]
#[test]
fn q3_unknown_single_file_edits_have_narrow_plans() {
    // Q3: a healthy native watcher visits only the edited regular file, without enumerating unchanged siblings.
    let f = Fixture::new();
    fs::write(f.root.join("untouched"), b"sibling").unwrap();
    let mut repo = f.attach("narrow");
    repo.enable_change_tracking();
    seal(&mut repo);
    for iteration in 0..8 {
        fs::write(f.root.join("a"), format!("edit {iteration}")).unwrap();
        let plan = repo.capture_plan(None).unwrap();
        assert!(
            !plan.full,
            "ordinary non-linked files do not require a fallback"
        );
        assert_eq!(plan.paths, vec![f.root.join("a")]);
        repo.seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
            .unwrap();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn q3_writes_through_new_external_hardlinks_are_not_missed() {
    // Q3/M2: a new alias outside watched directories must not hide a subsequent write to an included inode.
    let f = Fixture::new();
    let mut repo = f.attach("external-alias");
    repo.enable_change_tracking();
    seal(&mut repo);
    let alias = f.data.join("outside-alias");
    fs::hard_link(f.root.join("a"), &alias).unwrap();
    fs::write(&alias, b"outside alias write").unwrap();
    let plan = repo.capture_plan(None).unwrap();
    assert!(plan.full);
    let id = repo
        .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .unwrap();
    assert_eq!(
        snapshot_bytes(&repo, &id, "a"),
        Some(b"outside alias write".to_vec())
    );
}

#[cfg(target_os = "linux")]
#[test]
fn q3_completed_mapped_writes_are_captured_after_the_writer_closes() {
    // Q3: a command that updates a mapped file and exits must be captured even without an IN_MODIFY event.
    use std::os::fd::AsRawFd;
    let f = Fixture::new();
    let mut repo = f.attach("mapped");
    repo.enable_change_tracking();
    seal(&mut repo);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.root.join("a"))
        .unwrap();
    // The mapping spans this known five-byte fixture and remains live until its writes are flushed.
    unsafe {
        let address = libc::mmap(
            std::ptr::null_mut(),
            5,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        );
        assert_ne!(address, libc::MAP_FAILED);
        std::ptr::copy_nonoverlapping(b"other".as_ptr(), address.cast::<u8>(), 5);
        assert_eq!(libc::msync(address, 5, libc::MS_SYNC), 0);
        assert_eq!(libc::munmap(address, 5), 0);
    }
    drop(file);
    let id = seal(&mut repo);
    assert_eq!(snapshot_bytes(&repo, &id, "a"), Some(b"other".to_vec()));
}
