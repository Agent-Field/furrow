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
#[cfg(unix)]
fn capture_plan_skips_permission_denied_directories() {
    // Root bypasses mode bits, so it cannot reproduce this permission failure.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let f = Fixture::new();
    let blocked = f.root.join("blocked");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("hidden"), b"unreadable subtree").unwrap();
    let mut repo = f.attach("chat");
    let permissions = fs::metadata(&blocked).unwrap().permissions();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0)).unwrap();
    assert_eq!(
        fs::read_dir(&blocked).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    let result = repo.capture_plan(None);
    // Regaining access must not expand the screened capture scope.
    fs::set_permissions(&blocked, permissions).unwrap();
    let capture = result
        .as_ref()
        .ok()
        .map(|plan| repo.seal_plan(None, SnapshotTrigger::AgentRun, &plan.token));
    let plan = result.expect("an unreadable directory must not abort capture planning");
    capture
        .unwrap()
        .expect("the plan must remain usable for capture");
    let root = f.root.canonicalize().unwrap();
    assert!(plan.full);
    assert!(plan.paths.contains(&root.join("a")));
    assert!(!plan
        .paths
        .iter()
        .any(|path| path.starts_with(root.join("blocked"))));
    let json = serde_json::to_value(&plan).unwrap();
    assert_eq!(json["skipped"], serde_json::json!([root.join("blocked")]));
}

#[test]
#[cfg(unix)]
fn capture_plan_skips_permission_denied_entries_in_a_delta() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let f = Fixture::new();
    let blocked = f.root.join("blocked");
    fs::create_dir_all(blocked.join("nested")).unwrap();
    fs::write(blocked.join("nested/hidden"), b"unreadable entry").unwrap();
    let mut repo = f.attach("chat");
    seal(&mut repo);
    let permissions = fs::metadata(&blocked).unwrap().permissions();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0)).unwrap();
    let result = repo.capture_plan(Some(&[
        PathBuf::from("a"),
        PathBuf::from("blocked/nested/hidden"),
    ]));
    let capture = result
        .as_ref()
        .ok()
        .map(|plan| repo.seal_plan(None, SnapshotTrigger::AgentRun, &plan.token));
    fs::set_permissions(&blocked, permissions).unwrap();
    let plan = result.expect("an inaccessible entry must not abort a delta plan");
    let root = f.root.canonicalize().unwrap();
    assert!(!plan.full);
    assert_eq!(plan.paths, [root.join("a")]);
    assert_eq!(plan.skipped, [root.join("blocked/nested/hidden")]);
    capture
        .unwrap()
        .expect("skipped ancestors must not be read");
}

#[test]
fn capture_plan_propagates_non_permission_io_errors() {
    let f = Fixture::new();
    let mut repo = f.attach("chat");
    seal(&mut repo);
    let error = repo
        .capture_plan(Some(&[PathBuf::from("a/child")]))
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::NotADirectory
    );
}

#[test]
fn capture_plan_without_skips_preserves_the_wire_format() {
    let f = Fixture::new();
    let plan = f.attach("chat").capture_plan(None).unwrap();
    let json = serde_json::to_value(&plan).unwrap();
    assert!(json.get("skipped").is_none());
    let decoded: CapturePlan = serde_json::from_value(json).unwrap();
    assert!(decoded.skipped.is_empty());
    assert_eq!(decoded.paths, plan.paths);
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
    // F8 S1/S4/C28/C9: a stale legacy index cannot replace the authoritative tree or drop a completed capture.
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
    assert!(plan
        .paths
        .contains(&f.root.canonicalize().unwrap().join("b")));
    assert_eq!(repo.status().unwrap().head, Some(id_hex(&second)));
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

#[cfg(target_os = "linux")]
#[test]
fn p3_new_regular_files_do_not_repeat_the_cold_walk() {
    // P3/P5: a new regular file must be screened and captured without discarding healthy directory watches.
    let f = Fixture::new();
    let mut repo = f.attach("new-files");
    repo.enable_change_tracking();
    seal(&mut repo);
    for i in 0..8 {
        let path = f.root.join(format!("created-{i}"));
        fs::write(&path, format!("new bytes {i}")).unwrap();
        let plan = repo.capture_plan(None).unwrap();
        assert!(!plan.full, "regular-file creation must not restart warm-up");
        assert!(plan.paths.contains(&path));
        let id = repo
            .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
            .unwrap();
        assert_eq!(
            snapshot_bytes(&repo, &id, &format!("created-{i}")),
            Some(format!("new bytes {i}").into_bytes())
        );
    }
    // P5: the newly registered inode still reports a write through an alias outside the workspace.
    let alias = f.data.join("outside-new-alias");
    fs::hard_link(f.root.join("created-7"), &alias).unwrap();
    fs::write(alias, b"outside change").unwrap();
    let plan = repo.capture_plan(None).unwrap();
    assert!(plan.full);
    let id = repo
        .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .unwrap();
    assert_eq!(
        snapshot_bytes(&repo, &id, "created-7"),
        Some(b"outside change".to_vec())
    );
}

#[cfg(target_os = "linux")]
#[test]
fn p3_one_full_capture_reestablishes_a_changed_directory_baseline() {
    // P3/P5: topology uncertainty widens once, and the replacement watches precede the full screened capture.
    let f = Fixture::new();
    let mut repo = f.attach("topology");
    repo.enable_change_tracking();
    seal(&mut repo);
    fs::create_dir(f.root.join("new-directory")).unwrap();
    fs::write(f.root.join("new-directory/a"), b"new child").unwrap();
    let plan = repo.capture_plan(None).unwrap();
    assert!(plan.full);
    repo.seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .unwrap();
    let plan = repo.capture_plan(None).unwrap();
    assert!(
        !plan.full,
        "one complete capture after watch registration establishes the baseline"
    );
    fs::write(f.root.join("new-directory/a"), b"later child").unwrap();
    let plan = repo.capture_plan(None).unwrap();
    assert!(!plan.full);
    assert!(plan.paths.contains(&f.root.join("new-directory/a")));
    let id = repo
        .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
        .unwrap();
    assert_eq!(
        snapshot_bytes(&repo, &id, "new-directory/a"),
        Some(b"later child".to_vec())
    );
}

#[test]
fn p4_sync_and_independent_index_folding_preserve_completed_captures() {
    // P4/P5/C28: checkpointing advisory indexes separately never loses an acknowledged head or its bytes.
    let f = Fixture::new();
    for i in 0..4096 {
        fs::write(f.root.join(format!("{i:04}-{}", "x".repeat(92))), b"small").unwrap();
    }
    let mut repo = f.attach("checkpoint-worker");
    repo.store.defer_checkpoints().unwrap();
    repo.enable_change_tracking();
    let first = seal(&mut repo);
    let root = repo.store_root().to_owned();
    let mut group = crate::store::GroupCommit::open(&root).unwrap();
    for i in 0..3 {
        fs::write(f.root.join("a"), format!("committed {i}")).unwrap();
        repo.defer_durability().unwrap();
        let plan = repo.capture_plan(None).unwrap();
        let id = repo
            .seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
            .unwrap();
        assert_ne!(first, id);
        repo.sync().unwrap();
        group.checkpoint_indexes().unwrap();
        let reopened = f.attach("checkpoint-worker");
        assert_eq!(reopened.status().unwrap().head, Some(id_hex(&id)));
        assert_eq!(
            snapshot_bytes(&reopened, &id, "a"),
            Some(format!("committed {i}").into_bytes())
        );
    }
    // F8 S1/S4/C28: a live durability worker retains no per-timeline path database on either platform.
    let index = repo.workspace_data_dir().join("paths.sqlite3");
    drop(repo);
    assert!(!index.exists());
    let mut reopened = f.attach("checkpoint-worker");
    let id = seal(&mut reopened);
    assert_eq!(
        snapshot_bytes(&reopened, &id, "a"),
        Some(b"committed 2".to_vec())
    );
}

#[test]
fn f8_s1_s4_reopening_reconstructs_delta_paths_and_preserves_link_groups() {
    // F8 S1/S4/C28: trees alone reconstruct paths after reopen, including unchanged hard-link groups.
    let f = Fixture::new();
    fs::create_dir(f.root.join("nested")).unwrap();
    fs::write(f.root.join("nested/b"), b"unchanged").unwrap();
    fs::hard_link(f.root.join("nested/b"), f.root.join("nested/c")).unwrap();
    let mut repo = f.attach("shared-paths");
    let first = seal(&mut repo);
    assert!(!repo.workspace_data_dir().join("paths.sqlite3").exists());
    let first_snapshot: Snapshot = repo
        .store
        .read_struct(&first, ObjectKind::Snapshot)
        .unwrap();
    assert_eq!(first_snapshot.hardlinks.len(), 1);
    drop(repo);
    let mut repo = f.attach("shared-paths");
    fs::write(f.root.join("a"), b"edited").unwrap();
    let next = repo
        .snapshot_changed_paths(None, SnapshotTrigger::AgentRun, &[PathBuf::from("a")])
        .unwrap();
    let next_snapshot: Snapshot = repo.store.read_struct(&next, ObjectKind::Snapshot).unwrap();
    assert_eq!(first_snapshot.hardlinks, next_snapshot.hardlinks);
    assert_eq!(
        repo.lookup_tree_path(&first_snapshot.root_tree, b"nested/b")
            .unwrap(),
        repo.lookup_tree_path(&next_snapshot.root_tree, b"nested/b")
            .unwrap()
    );
    assert_eq!(snapshot_bytes(&repo, &next, "a"), Some(b"edited".to_vec()));
}

#[test]
fn f8_s1_s2_identical_chats_share_every_tree_and_edits_replace_small_pages() {
    // F8 S1/S2: identical folders add no trees, and a single-file edit stages at most 20 KB of objects.
    let f = Fixture::new();
    for d in 0..10 {
        let dir = f.root.join(format!("d{d:03}"));
        fs::create_dir(&dir).unwrap();
        for n in 0..100 {
            fs::write(dir.join(format!("f{n:03}")), b"content").unwrap();
        }
    }
    let mut first = f.attach("first");
    let original = seal(&mut first);
    let original_tree: Snapshot = first
        .store
        .read_struct(&original, ObjectKind::Snapshot)
        .unwrap();
    let mut second = f.attach("second");
    let before = fs::metadata(second.store_root().join("packs/pack-000001.agp"))
        .unwrap()
        .len();
    let shared = seal(&mut second);
    let shared_tree: Snapshot = second
        .store
        .read_struct(&shared, ObjectKind::Snapshot)
        .unwrap();
    let after = fs::metadata(second.store_root().join("packs/pack-000001.agp"))
        .unwrap()
        .len();
    assert_eq!(original_tree.root_tree, shared_tree.root_tree);
    assert!(after - before <= 100_000, "{} bytes", after - before);
    assert!(!second.workspace_data_dir().join("paths.sqlite3").exists());
    fs::write(f.root.join("d000/f000"), b"changed").unwrap();
    second
        .snapshot_changed_paths(
            None,
            SnapshotTrigger::AgentRun,
            &[PathBuf::from("d000/f000")],
        )
        .unwrap();
    let edited = fs::metadata(second.store_root().join("packs/pack-000001.agp"))
        .unwrap()
        .len();
    assert!(edited - after <= 20_000, "{} bytes", edited - after);
}

#[test]
fn f8_s4_c25_legacy_indexes_are_never_removed_or_used_as_snapshot_truth() {
    // F8 S4/C25: an old index is preserved byte for byte, while the saved tree remains authoritative.
    let f = Fixture::new();
    let mut repo = f.attach("legacy-index");
    let saved = seal(&mut repo);
    let path = repo.workspace_data_dir().join("paths.sqlite3");
    fs::write(&path, b"legacy index left untouched").unwrap();
    drop(repo);
    let mut repo = f.attach("legacy-index");
    let next = repo
        .snapshot_changed_paths(None, SnapshotTrigger::AgentRun, &[])
        .unwrap();
    assert_eq!(
        snapshot_bytes(&repo, &saved, "a"),
        snapshot_bytes(&repo, &next, "a")
    );
    assert_eq!(fs::read(path).unwrap(), b"legacy index left untouched");
}

#[test]
#[cfg(windows)]
fn f8_s6_native_full_walk_measurement() {
    // F8 S6: native full walks report cold, unchanged and one-edit cost with exact restored bytes.
    let f = Fixture::new();
    // Exclude the helper's seed so future runs contain exactly 10,000 files.
    fs::remove_file(f.root.join("a")).unwrap();
    for d in 0..100 {
        let dir = f.root.join(format!("d{d:03}"));
        fs::create_dir(&dir).unwrap();
        for n in 0..100 {
            fs::write(dir.join(format!("f{n:03}")), b"content").unwrap();
        }
    }
    let mut repo = f.attach("native-perf");
    repo.store.retain_file_cache();
    let mut measurements = Vec::new();
    let start = Instant::now();
    seal(&mut repo);
    measurements
        .push(serde_json::json!({"kind": "first", "ms": start.elapsed().as_secs_f64() * 1000.0}));
    for sample in 0..5 {
        let start = Instant::now();
        seal(&mut repo);
        measurements.push(serde_json::json!({"kind": "unchanged", "sample": sample, "ms": start.elapsed().as_secs_f64() * 1000.0}));
        fs::write(f.root.join("d000/f000"), format!("edit {sample}")).unwrap();
        let start = Instant::now();
        let saved = seal(&mut repo);
        measurements.push(serde_json::json!({"kind": "edit", "sample": sample, "ms": start.elapsed().as_secs_f64() * 1000.0}));
        assert_eq!(
            snapshot_bytes(&repo, &saved, "d000/f000"),
            Some(format!("edit {sample}").into_bytes())
        );
    }
    if let Some(output) = std::env::var_os("FURROW_PORTABLE_OUT") {
        let output = PathBuf::from(output);
        fs::create_dir_all(&output).unwrap();
        fs::write(
            output.join("f8-native-perf.json"),
            serde_json::to_vec_pretty(&measurements).unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn f8_s4_older_writer_cannot_bind_an_upgraded_store() {
    // F8 S4: a connection without the new format capability refuses workspace binding before any file restore.
    let f = Fixture::new();
    let mut repo = f.attach("writer-version");
    seal(&mut repo);
    let old = rusqlite::Connection::open(repo.store_root().join("catalog.sqlite3")).unwrap();
    let error = old
        .execute(
            "INSERT OR IGNORE INTO workspaces(id, root) VALUES('old', x'6f6c64')",
            [],
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("requires_furrow_shared_paths_v1"),
        "{error}"
    );
    assert_eq!(fs::read(f.root.join("a")).unwrap(), b"first");
    seal(&mut repo);
}

#[test]
fn f8_c9_restore_invalidation_survives_reopening_and_stale_head_hints() {
    // F8 S4/C9/C28: even no-op rewind inspection preserves a full-screen boundary across processes.
    for apply in [false, true] {
        let f = Fixture::new();
        let mut repo = f.attach("restored");
        let original = seal(&mut repo);
        if apply {
            repo.rewind(&original, &[], false).unwrap();
        } else {
            assert!(repo.plan_rewind(&original, &[]).unwrap().changes.is_empty());
        }
        drop(repo);
        fs::write(f.root.join(".env"), b"new candidate after restore").unwrap();
        let mut repo = f.attach("restored");
        let plan = repo.capture_plan(Some(&[])).unwrap();
        assert!(plan.full);
        assert!(plan
            .paths
            .contains(&f.root.join(".env").canonicalize().unwrap()));
        let next = seal(&mut repo);
        let marker = repo.workspace_data_dir().join("paths.head");
        assert_eq!(
            fs::read(&marker).unwrap(),
            repo.indexed_head_token(&next).unwrap().as_bytes()
        );
        drop(repo);
        #[cfg(not(windows))]
        assert!(!f.attach("restored").capture_plan(Some(&[])).unwrap().full);
        for stale in [
            b"invalid\n".to_vec(),
            b"torn".to_vec(),
            format!("{}\n", id_hex(&original)).into_bytes(),
        ] {
            fs::write(&marker, stale).unwrap();
            assert!(f.attach("restored").capture_plan(Some(&[])).unwrap().full);
        }
    }
}
