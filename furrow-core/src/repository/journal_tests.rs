use super::*;
use crate::chunker::with_journal_chunks;
use std::fs::OpenOptions;

type VerificationHook = Option<Box<dyn FnOnce()>>;

thread_local! {
    static AFTER_CAPTURE: std::cell::RefCell<VerificationHook> = const { std::cell::RefCell::new(None) };
    static DURING_FALLBACK: std::cell::RefCell<VerificationHook> = const { std::cell::RefCell::new(None) };
}

pub(super) fn after_verification_capture() {
    let hook = AFTER_CAPTURE.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

pub(super) struct VerificationReader<R>(pub R);

impl<R: std::io::Read> std::io::Read for VerificationReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.0.read(buffer)?;
        let hook = DURING_FALLBACK.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
        Ok(count)
    }
}

fn with_verification_hook<T>(
    fallback: bool,
    hook: impl FnOnce() + 'static,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            AFTER_CAPTURE.with(|slot| slot.replace(None));
            DURING_FALLBACK.with(|slot| slot.replace(None));
        }
    }
    let _reset = Reset;
    let hook: VerificationHook = Some(Box::new(hook));
    if fallback {
        DURING_FALLBACK.with(|slot| slot.replace(hook));
    } else {
        AFTER_CAPTURE.with(|slot| slot.replace(hook));
    }
    let result = run();
    assert!(AFTER_CAPTURE.with(|slot| slot.borrow().is_none()));
    assert!(DURING_FALLBACK.with(|slot| slot.borrow().is_none()));
    result
}

fn journal_bytes() -> Vec<u8> {
    let mut state = 42_u64;
    (0..500_000)
        .map(|index| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            if index % 12000 == 11999 {
                b'\n'
            } else {
                (state >> 33) as u8
            }
        })
        .collect()
}

fn fresh_blob(repo: &FurrowRepository, path: &Path, enabled: bool) -> ObjectId {
    with_journal_chunks(enabled, || {
        let chunked = repo.store_chunks(File::open(path).unwrap(), path).unwrap();
        repo.store
            .put_struct(
                ObjectKind::Blob,
                &Blob {
                    chunks: chunked.chunks,
                    total_len: chunked.total_len,
                },
            )
            .unwrap()
    })
}

fn snapshot(repo: &FurrowRepository, head: &ObjectId) -> Snapshot {
    repo.store.read_struct(head, ObjectKind::Snapshot).unwrap()
}

fn entry(repo: &FurrowRepository, head: &ObjectId, path: &[u8]) -> TreeEntry {
    repo.lookup_tree_path(&snapshot(repo, head).root_tree, path)
        .unwrap()
        .unwrap()
}

#[test]
fn journal_r1_cross_profile_rewind_verifies_including_composed_cell() {
    for writer_enabled in [false, true] {
        for composed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("tree");
            let data = temp.path().join("data");
            let cell = temp.path().join("private");
            fs::create_dir(&root).unwrap();
            fs::create_dir(&cell).unwrap();
            let path = if composed {
                cell.join("worker.ndjson")
            } else {
                root.join("transcript.JSONL")
            };
            let relative: &[u8] = if composed {
                b".cell/worker.ndjson"
            } else {
                b"transcript.JSONL"
            };
            let bytes = journal_bytes();
            fs::write(&path, &bytes).unwrap();
            let mut repo = FurrowRepository::attach_timeline_in(
                &data,
                &root,
                Some("chat"),
                composed.then_some(cell),
            )
            .unwrap();
            let original = with_journal_chunks(writer_enabled, || {
                repo.seal(None, SnapshotTrigger::Manual, None).unwrap()
            });
            let expected = entry(&repo, &original, relative);
            let other_blob = fresh_blob(&repo, &path, !writer_enabled);
            assert_ne!(expected.target, Some(other_blob));
            with_journal_chunks(!writer_enabled, || {
                fs::write(&path, b"different workspace contents\n").unwrap();
                repo.seal(None, SnapshotTrigger::Manual, None).unwrap();
                repo.rewind(&original, &[], false).unwrap();
                assert!(repo.path_matches_entry(&path, &expected).unwrap());
            });
            assert_eq!(fs::read(&path).unwrap(), bytes);
            if composed {
                assert!(!root.join(".cell").exists());
            }
            let expected = expected.target.unwrap();
            for changed in [
                &bytes[..bytes.len() - 1],
                &[bytes.as_slice(), b"extra"].concat()[..],
            ] {
                assert!(
                    !crate::blob::matches_reader(&repo.store, &expected, None, changed).unwrap()
                );
            }
            let mut corrupt = bytes;
            corrupt[100] ^= 1;
            assert!(
                !crate::blob::matches_reader(&repo.store, &expected, None, corrupt.as_slice())
                    .unwrap()
            );
            let mut invalid: Blob = repo.store.read_struct(&expected, ObjectKind::Blob).unwrap();
            invalid.total_len += 1;
            let invalid = repo.store.put_struct(ObjectKind::Blob, &invalid).unwrap();
            assert!(!crate::blob::matches_reader(
                &repo.store,
                &invalid,
                None,
                fs::read(&path).unwrap().as_slice(),
            )
            .unwrap());
        }
    }
}

#[test]
fn journal_v1_fallback_rejects_rewrite_or_replacement_during_verification() {
    for verifier_enabled in [false, true] {
        for replace in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("tree");
            fs::create_dir(&root).unwrap();
            let path = root.join("transcript.jsonl");
            let bytes = journal_bytes();
            fs::write(&path, &bytes).unwrap();
            let mut repo = FurrowRepository::attach_timeline_in(
                &temp.path().join("data"),
                &root,
                Some("chat"),
                None,
            )
            .unwrap();
            let head = with_journal_chunks(!verifier_enabled, || {
                repo.seal(None, SnapshotTrigger::Manual, None).unwrap()
            });
            let expected = entry(&repo, &head, b"transcript.jsonl");
            let expected_blob: Blob = repo
                .store
                .read_struct(&expected.target.unwrap(), ObjectKind::Blob)
                .unwrap();
            assert!(expected_blob.chunks.len() > 1);
            assert_ne!(
                expected.target,
                Some(fresh_blob(&repo, &path, verifier_enabled))
            );
            // A restored path outside the workspace forces fresh chunking instead of capture's reuse guard.
            let restored = temp.path().join("restored.jsonl");
            fs::copy(&path, &restored).unwrap();
            filetime::set_file_mtime(
                &restored,
                FileTime::from_unix_time(expected.mtime_secs, expected.mtime_nanos),
            )
            .unwrap();
            let changed_path = restored.clone();
            let replacement = temp.path().join("replacement.jsonl");
            if replace {
                fs::copy(&restored, &replacement).unwrap();
                filetime::set_file_mtime(
                    &replacement,
                    FileTime::from_unix_time(expected.mtime_secs, expected.mtime_nanos),
                )
                .unwrap();
            }
            let accepted = with_journal_chunks(verifier_enabled, || {
                with_verification_hook(
                    true,
                    move || {
                        if replace {
                            crate::platform::rename(&replacement, &changed_path).unwrap();
                        } else {
                            let mut writer =
                                OpenOptions::new().write(true).open(&changed_path).unwrap();
                            writer.write_all(&[bytes[0] ^ 1]).unwrap();
                            writer.flush().unwrap();
                        }
                    },
                    || repo.path_matches_entry(&restored, &expected),
                )
            });
            assert!(!accepted.unwrap_or(false));
        }
    }
}

#[test]
fn journal_v2_matching_ids_keep_legacy_acceptance_and_xattr_verification() {
    for enabled in [false, true] {
        with_journal_chunks(enabled, || {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("tree");
            fs::create_dir(&root).unwrap();
            let path = root.join("transcript.jsonl");
            fs::write(&path, journal_bytes()).unwrap();
            let mut repo = FurrowRepository::attach_timeline_in(
                &temp.path().join("data"),
                &root,
                Some("chat"),
                None,
            )
            .unwrap();
            let head = repo.seal(None, SnapshotTrigger::Manual, None).unwrap();
            let mut expected = entry(&repo, &head, b"transcript.jsonl");
            expected.xattrs = Some([0x91; 32]);
            assert!(repo.path_matches_entry(&path, &expected).is_err());
            expected.xattrs = None;
            let moved = path.clone();
            assert!(with_verification_hook(
                false,
                move || {
                    fs::rename(&moved, moved.with_extension("renamed")).unwrap();
                },
                || repo.path_matches_entry(&path, &expected)
            )
            .unwrap());
        });
    }
}

#[test]
fn journal_r2_r5_upgrade_keeps_cached_and_parent_blobs_without_rereading_full_capture() {
    for composed in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        let data = temp.path().join("data");
        let cell = temp.path().join("private");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&cell).unwrap();
        let path = if composed {
            cell.join("worker.ndjson")
        } else {
            root.join("transcript.JSONL")
        };
        let relative: &[u8] = if composed {
            b".cell/worker.ndjson"
        } else {
            b"transcript.JSONL"
        };
        let bytes = journal_bytes();
        fs::write(&path, &bytes).unwrap();
        fs::write(root.join("workspace.bin"), &bytes).unwrap();
        let overlay = composed.then_some(cell);
        let mut repo =
            FurrowRepository::attach_timeline_in(&data, &root, Some("chat"), overlay.clone())
                .unwrap();
        let first = with_journal_chunks(false, || {
            repo.seal(None, SnapshotTrigger::Manual, None).unwrap()
        });
        let original = entry(&repo, &first, relative).target;
        assert_ne!(original, Some(fresh_blob(&repo, &path, true)));
        for name in [relative, b"workspace.bin"] {
            let mut row = repo
                .store
                .cached_file(&repo.workspace_id, name)
                .unwrap()
                .unwrap();
            row.cached_at_nanos = i64::MAX;
            repo.store
                .cache_file(&repo.workspace_id, name, &row)
                .unwrap();
        }
        let generation = repo.store_generation().unwrap();
        drop(repo);
        let mut repo =
            FurrowRepository::open_timeline_in(&data, &root, overlay, Some("chat")).unwrap();
        with_journal_chunks(true, || {
            assert_eq!(repo.store_generation().unwrap(), generation);
            assert!(!repo.capture_plan(Some(&[])).unwrap().full);
            // A full walk must preserve both settled cache rows, proving neither file was re-read.
            for _ in 0..2 {
                let head = repo.seal(None, SnapshotTrigger::Manual, None).unwrap();
                assert_eq!(entry(&repo, &head, relative).target, original);
                for name in [relative, b"workspace.bin"] {
                    assert_eq!(
                        repo.store
                            .cached_file(&repo.workspace_id, name)
                            .unwrap()
                            .unwrap()
                            .cached_at_nanos,
                        i64::MAX
                    );
                }
            }
            // A metadata change invalidates the stat shortcut but must preserve the cached Blob's representation.
            filetime::set_file_mtime(&path, FileTime::from_unix_time(1_700_000_000, 0)).unwrap();
            let head = repo.seal(None, SnapshotTrigger::Manual, None).unwrap();
            assert_eq!(entry(&repo, &head, relative).target, original);
        });
        let store_root = repo.store.root().to_owned();
        drop(repo);
        let conn = rusqlite::Connection::open(store_root.join("catalog.sqlite3")).unwrap();
        conn.execute("DELETE FROM stat_cache WHERE path = ?1", [relative])
            .unwrap();
        drop(conn);
        let mut repo = FurrowRepository::open_timeline_in(
            &data,
            &root,
            if composed {
                Some(temp.path().join("private"))
            } else {
                None
            },
            Some("chat"),
        )
        .unwrap();
        with_journal_chunks(true, || {
            let head = repo.seal(None, SnapshotTrigger::Manual, None).unwrap();
            assert_eq!(entry(&repo, &head, relative).target, original);
            let mut changed = bytes;
            changed[100] ^= 1;
            fs::write(&path, changed).unwrap();
            let head = repo.seal(None, SnapshotTrigger::Manual, None).unwrap();
            assert_eq!(
                entry(&repo, &head, relative).target,
                Some(fresh_blob(&repo, &path, true))
            );
            assert_ne!(entry(&repo, &head, relative).target, original);
        });
    }
}

#[test]
fn journal_k5_profile_switch_fast_forward_keeps_incoming_blob() {
    let temp = tempfile::tempdir().unwrap();
    let roots = [temp.path().join("a"), temp.path().join("b")];
    let mut repos = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        fs::create_dir(root).unwrap();
        fs::write(root.join("transcript.jsonl"), journal_bytes()).unwrap();
        let mut repo = FurrowRepository::attach_timeline_in(
            &temp.path().join(format!("data-{index}")),
            root,
            Some("chat"),
            None,
        )
        .unwrap();
        with_journal_chunks(false, || {
            repo.seal(None, SnapshotTrigger::Manual, None).unwrap()
        });
        repo.pair(
            &temp.path().join("remote"),
            "journal",
            Some(&"11".repeat(32)),
        )
        .unwrap();
        repos.push(repo);
    }
    let mut b = repos.pop().unwrap();
    let mut a = repos.pop().unwrap();
    with_journal_chunks(false, || {
        a.sync_push(false, None).unwrap();
        b.sync_pull(true, None).unwrap();
        b.seal(None, SnapshotTrigger::Manual, None).unwrap();
    });
    let old = b.store.workspace_head(&b.workspace_id).unwrap().unwrap();
    let old_blob = entry(&b, &old, b"transcript.jsonl").target;
    let mut cached = b
        .store
        .cached_file(&b.workspace_id, b"transcript.jsonl")
        .unwrap()
        .unwrap();
    cached.cached_at_nanos = i64::MAX;
    b.store
        .cache_file(&b.workspace_id, b"transcript.jsonl", &cached)
        .unwrap();
    with_journal_chunks(true, || {
        let head = b.seal(None, SnapshotTrigger::Manual, None).unwrap();
        assert_eq!(entry(&b, &head, b"transcript.jsonl").target, old_blob);
        assert_eq!(
            b.store
                .cached_file(&b.workspace_id, b"transcript.jsonl")
                .unwrap()
                .unwrap()
                .cached_at_nanos,
            i64::MAX
        );
    });
    with_journal_chunks(false, || {
        OpenOptions::new()
            .append(true)
            .open(roots[0].join("transcript.jsonl"))
            .unwrap()
            .write_all(b"appended on A\n")
            .unwrap();
        a.sync_push(false, None).unwrap();
    });
    with_journal_chunks(true, || {
        assert!(matches!(
            b.sync_pull(false, None).unwrap().disposition,
            SyncDisposition::FastForwarded
        ));
        assert_eq!(
            fs::read(roots[0].join("transcript.jsonl")).unwrap(),
            fs::read(roots[1].join("transcript.jsonl")).unwrap()
        );
        let incoming = b.store.workspace_head(&b.workspace_id).unwrap().unwrap();
        // Invalidating the stat row forces the enabled guard to check the incoming legacy layout.
        filetime::set_file_mtime(
            roots[1].join("transcript.jsonl"),
            FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();
        let head = b.seal(None, SnapshotTrigger::Manual, None).unwrap();
        assert_eq!(
            entry(&b, &head, b"transcript.jsonl").target,
            entry(&b, &incoming, b"transcript.jsonl").target
        );
    });
}

fn expired_journal(enabled: bool, writer_enabled: bool, changed: bool, ambiguous_root: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("tree");
    let data = temp.path().join("data");
    fs::create_dir_all(root.join("logs")).unwrap();
    let path = root.join("logs/transcript.jsonl");
    let relative = b"logs/transcript.jsonl";
    let bytes = journal_bytes();
    fs::write(&path, &bytes).unwrap();
    let mut repo = FurrowRepository::attach_timeline_in(&data, &root, Some("chat"), None).unwrap();
    let tree = with_journal_chunks(writer_enabled, || {
        repo.capture_tree(None, &repo.load_policy().unwrap())
            .unwrap()
    });
    let old_time = now().0 - 2 * 24 * 60 * 60;
    let old = Snapshot {
        root_tree: tree,
        parent: None,
        merge_parents: Vec::new(),
        sealed_at_secs: old_time,
        sealed_at_nanos: 0,
        quality: SealQuality::Quiescent,
        trigger: SnapshotTrigger::Manual,
        label: None,
        sqlite_backups: Vec::new(),
        claims: Vec::new(),
        excluded_paths: Vec::new(),
        hardlinks: Vec::new(),
        ignored: None,
    };
    let old = repo.store.put_struct(ObjectKind::Snapshot, &old).unwrap();
    repo.store
        .publish_snapshot(
            &repo.workspace_id,
            old,
            old_time,
            None,
            SnapshotTrigger::Manual,
        )
        .unwrap();
    let expected = entry(&repo, &old, relative).target.unwrap();
    let blob: Blob = repo.store.read_struct(&expected, ObjectKind::Blob).unwrap();
    fs::remove_file(&path).unwrap();
    let deleted = with_journal_chunks(false, || {
        repo.seal(None, SnapshotTrigger::Manual, None).unwrap()
    });
    assert!(repo
        .lookup_tree_path(&snapshot(&repo, &deleted).root_tree, relative)
        .unwrap()
        .is_none());
    crate::gc::collect(&mut repo.store, false).unwrap();
    assert!(repo.store.contains_object(&expected).unwrap());
    assert_eq!(
        repo.store
            .cached_file(&repo.workspace_id, relative)
            .unwrap()
            .unwrap()
            .blob_id,
        expected
    );
    assert!(blob
        .chunks
        .iter()
        .all(|chunk| !repo.store.contains_object(&chunk.id).unwrap()));
    // The verifier needs only the manifest and working bytes even after every stored chunk expires.
    assert!(crate::blob::matches_reader(&repo.store, &expected, None, bytes.as_slice()).unwrap());
    assert!(blob
        .chunks
        .iter()
        .all(|chunk| !repo.store.contains_object(&chunk.id).unwrap()));
    if ambiguous_root {
        let mut second = repo
            .attach_timeline_shared(&root, Some("second"), None)
            .unwrap();
        with_journal_chunks(false, || {
            second.seal(None, SnapshotTrigger::Manual, None).unwrap()
        });
        let other_root = temp.path().join("other-tree");
        fs::create_dir(&other_root).unwrap();
        fs::write(other_root.join("transcript.jsonl"), &bytes).unwrap();
        let mut other = repo
            .attach_timeline_shared(&other_root, Some("other"), None)
            .unwrap();
        let other_head = with_journal_chunks(true, || {
            other.seal(None, SnapshotTrigger::Manual, None).unwrap()
        });
        let other_blob = entry(&other, &other_head, b"transcript.jsonl")
            .target
            .unwrap();
        assert_ne!(other_blob, expected);
        let selected: Blob = repo
            .store
            .read_struct(&other_blob, ObjectKind::Blob)
            .unwrap();
        assert!(selected
            .chunks
            .iter()
            .all(|chunk| repo.store.contains_object(&chunk.id).unwrap()));
        for workspace in [&repo.workspace_id, &second.workspace_id] {
            assert_eq!(
                repo.store
                    .cached_file(workspace, relative)
                    .unwrap()
                    .unwrap()
                    .blob_id,
                expected
            );
        }
        assert!(blob
            .chunks
            .iter()
            .all(|chunk| !repo.store.contains_object(&chunk.id).unwrap()));
        let unattached = temp.path().join("unattached-tree");
        fs::create_dir(&unattached).unwrap();
        fs::write(unattached.join("transcript.jsonl"), &bytes).unwrap();
        with_journal_chunks(true, || {
            assert_eq!(
                crate::estimate::calculate(&unattached, &repo.store)
                    .unwrap()
                    .projected_new_chunk_bytes,
                0
            );
        });
    }
    if !enabled {
        let before = entry(&repo, &old, relative);
        let base = tree::write(&repo.store, vec![before.clone()]).unwrap();
        let mut after = before;
        let missing_chunk = ChunkRef {
            id: [0x77; 32],
            len: blob.total_len as u32,
        };
        after.target = Some(
            repo.store
                .put_struct(
                    ObjectKind::Blob,
                    &Blob {
                        chunks: vec![missing_chunk],
                        total_len: blob.total_len,
                    },
                )
                .unwrap(),
        );
        let changed = tree::write(&repo.store, vec![after]).unwrap();
        with_journal_chunks(false, || {
            let plan = merge::plan_trees(&repo.store, &base, &base, &changed).unwrap();
            assert!(plan.conflicts.is_empty());
            assert_eq!(plan.changes.len(), 1);
            assert!(!repo.working_trees_match(&base, &changed).unwrap());
        });
    }
    let mut recreated = bytes.clone();
    if changed {
        *recreated.last_mut().unwrap() ^= 1;
    }
    fs::write(&path, &recreated).unwrap();
    if ambiguous_root {
        with_journal_chunks(true, || {
            assert_eq!(
                crate::estimate::calculate_for_workspace(&root, &repo.store, "t-unattached")
                    .unwrap()
                    .projected_new_chunk_bytes,
                0
            );
        });
    }
    let catalog = repo.store.root().join("catalog.sqlite3");
    let chunk_payload = || {
        rusqlite::Connection::open(&catalog)
            .unwrap()
            .query_row(
                "SELECT COALESCE(SUM(len), 0) FROM objects WHERE kind = ?1",
                [ObjectKind::Chunk as u8],
                |row| row.get::<_, u64>(0),
            )
            .unwrap()
    };
    let payload_before = chunk_payload();
    let mut projected = 0;
    let head = with_journal_chunks(enabled, || {
        let estimate = crate::estimate::calculate(&root, &repo.store).unwrap();
        projected = estimate.projected_new_chunk_bytes;
        if !ambiguous_root {
            assert!(estimate.projected_new_chunk_bytes > 0);
        }
        assert!(blob
            .chunks
            .iter()
            .all(|chunk| !repo.store.contains_object(&chunk.id).unwrap()));
        repo.seal(None, SnapshotTrigger::Manual, None).unwrap()
    });
    if ambiguous_root {
        let stored = chunk_payload() - payload_before;
        assert!(stored > 0);
        assert!(
            projected >= stored,
            "estimated {projected}, stored {stored}"
        );
        assert_eq!(projected, recreated.len() as u64);
    }
    let actual = entry(&repo, &head, relative).target.unwrap();
    if changed {
        assert_ne!(actual, expected);
        assert!(blob
            .chunks
            .iter()
            .all(|chunk| !repo.store.contains_object(&chunk.id).unwrap()));
    } else if enabled {
        assert_eq!(actual, expected);
    } else {
        assert_eq!(actual == expected, !writer_enabled);
    }
    let captured: Blob = repo.store.read_struct(&actual, ObjectKind::Blob).unwrap();
    assert!(captured
        .chunks
        .iter()
        .all(|chunk| repo.store.contains_object(&chunk.id).unwrap()));
    if changed {
        let unique: BTreeMap<_, _> = captured
            .chunks
            .iter()
            .map(|chunk| (chunk.id, u64::from(chunk.len)))
            .collect();
        assert_eq!(projected, unique.values().sum::<u64>());
    }
    let keys = crate::sealer::Keys {
        cell_key: [0x11; 32],
        dedup: [0x22; 32],
    };
    let ledger = crate::exchange::ledger::Ledger::named(&data, "1234567890abcdef").unwrap();
    let report = crate::exchange::export::ExportJob {
        store: &repo.store,
        keys: &keys,
        head,
        ledger: &ledger,
        outbox: &temp.path().join("outbox"),
        max_frame: crate::exchange::export::DEFAULT_MAX_FRAME,
    }
    .run()
    .unwrap();
    let fresh_data = temp.path().join("fresh-data");
    let inbox = temp.path().join("inbox");
    fs::create_dir(&inbox).unwrap();
    for frame in report.frames {
        let raw = fs::read(frame.path).unwrap();
        for sealed in crate::exchange::frame::decode(&raw).unwrap().1 {
            fs::write(inbox.join(crate::model::id_hex(&sealed.rid)), sealed.bytes).unwrap();
        }
    }
    crate::exchange::ops::import(
        &fresh_data,
        &keys,
        &crate::model::id_hex(&head),
        "1234567890abcdef",
        &inbox,
        crate::exchange::ops::ImportMode::Strict,
    )
    .unwrap();
    let restored = temp.path().join("restored");
    fs::create_dir(&restored).unwrap();
    with_journal_chunks(enabled, || {
        crate::exchange::materialize::materialize(&fresh_data, &restored, None, head).unwrap()
    });
    assert_eq!(
        fs::read(restored.join("logs/transcript.jsonl")).unwrap(),
        recreated
    );
    // Completeness, export and fresh restoration must succeed before any helper can store reference chunks.
    if !enabled || changed {
        assert_eq!(actual, fresh_blob(&repo, &path, enabled));
    }
}

#[test]
fn journal_k2_gc_recreation_repairs_cached_chunks_before_reuse() {
    for writer in [false, true] {
        expired_journal(true, writer, false, false);
    }
}

#[test]
fn journal_k1_switch_off_gc_recreation_rechunks_like_036() {
    for writer in [false, true] {
        expired_journal(false, writer, false, false);
    }
}

#[test]
fn journal_v3_changed_last_byte_never_repairs_expired_legacy_prefix() {
    expired_journal(true, false, true, false);
}

#[test]
fn journal_e2_ambiguous_root_estimate_bounds_expired_legacy_repairs() {
    expired_journal(true, false, false, true);
}

#[test]
fn journal_k3_unchanged_upgrade_estimate_uses_cache_or_baseline() {
    for timeline in [None, Some("chat")] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        fs::create_dir(&root).unwrap();
        let path = root.join("transcript.jsonl");
        let bytes = journal_bytes();
        fs::write(&path, &bytes).unwrap();
        let data = temp.path().join("data");
        let cell = temp.path().join("private");
        fs::create_dir(&cell).unwrap();
        let mut repo = FurrowRepository::attach_timeline_in(
            &data,
            &root,
            timeline,
            timeline.is_none().then_some(cell),
        )
        .unwrap();
        let head = with_journal_chunks(false, || {
            repo.seal(None, SnapshotTrigger::Manual, None).unwrap()
        });
        let original = entry(&repo, &head, b"transcript.jsonl").target;
        filetime::set_file_mtime(&path, FileTime::from_unix_time(1_700_000_000, 0)).unwrap();
        for remove_cache in [false, true] {
            if remove_cache {
                rusqlite::Connection::open(repo.store.root().join("catalog.sqlite3"))
                    .unwrap()
                    .execute(
                        "DELETE FROM stat_cache WHERE path = ?1",
                        [b"transcript.jsonl".as_slice()],
                    )
                    .unwrap();
            }
            with_journal_chunks(true, || {
                let estimate = crate::estimate::calculate(&root, &repo.store).unwrap();
                assert_eq!(estimate.projected_new_chunk_bytes, 0);
                let captured = repo.seal(None, SnapshotTrigger::Manual, None).unwrap();
                assert_eq!(
                    entry(&repo, &captured, b"transcript.jsonl").target,
                    original
                );
            });
        }
        // Equal-length edits must roll back any partially verified legacy layout from the estimate.
        let mut changed = bytes;
        let last = changed.len() - 1;
        changed[last] ^= 1;
        fs::write(&path, &changed).unwrap();
        with_journal_chunks(true, || {
            let estimate = crate::estimate::calculate(&root, &repo.store).unwrap();
            let mut stream = ChunkStream::for_path(changed.as_slice(), &path);
            let mut missing = BTreeMap::new();
            while let Some(chunk) = stream.next_chunk().unwrap() {
                let id = crate::store::object_id(ObjectKind::Chunk, &chunk);
                if !repo.store.contains_object(&id).unwrap() {
                    missing.insert(id, chunk.len() as u64);
                }
            }
            assert_eq!(
                estimate.projected_new_chunk_bytes,
                missing.values().sum::<u64>()
            );
            let captured = repo.seal(None, SnapshotTrigger::Manual, None).unwrap();
            assert_eq!(
                entry(&repo, &captured, b"transcript.jsonl").target,
                Some(fresh_blob(&repo, &path, true))
            );
            assert_eq!(
                crate::estimate::calculate(&root, &repo.store)
                    .unwrap()
                    .projected_new_chunk_bytes,
                0
            );
        });
    }
}

#[test]
fn journal_v4_estimate_uses_selected_timeline_and_rejects_ambiguous_root_reuse() {
    for with_folder in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        fs::create_dir(&root).unwrap();
        let path = root.join("transcript.jsonl");
        let bytes = journal_bytes();
        fs::write(&path, &bytes).unwrap();
        let mut a =
            FurrowRepository::attach_timeline_in(&temp.path().join("data"), &root, Some("a"), None)
                .unwrap();
        let original = with_journal_chunks(false, || {
            a.seal(None, SnapshotTrigger::Manual, None).unwrap()
        });
        let mut changed = bytes.clone();
        *changed.last_mut().unwrap() ^= 1;
        fs::write(&path, changed).unwrap();
        let mut b = a.attach_timeline_shared(&root, Some("b"), None).unwrap();
        with_journal_chunks(false, || {
            b.seal(None, SnapshotTrigger::Manual, None).unwrap()
        });
        let _folder = with_folder.then(|| {
            let cell = temp.path().join("private");
            fs::create_dir(&cell).unwrap();
            let mut folder = a.attach_timeline_shared(&root, None, Some(cell)).unwrap();
            with_journal_chunks(false, || {
                folder.seal(None, SnapshotTrigger::Manual, None).unwrap()
            });
            folder
        });
        rusqlite::Connection::open(a.store.root().join("catalog.sqlite3"))
            .unwrap()
            .execute(
                "DELETE FROM stat_cache WHERE path = ?1",
                [b"transcript.jsonl".as_slice()],
            )
            .unwrap();
        fs::write(&path, &bytes).unwrap();
        with_journal_chunks(true, || {
            let selected_a =
                crate::estimate::calculate_for_workspace(&root, &a.store, &a.workspace_id).unwrap();
            assert_eq!(selected_a.projected_new_chunk_bytes, 0);
            let selected_b =
                crate::estimate::calculate_for_workspace(&root, &b.store, &b.workspace_id).unwrap();
            assert!(selected_b.projected_new_chunk_bytes > 0);
            let root_only = crate::estimate::calculate(&root, &b.store).unwrap();
            assert_eq!(root_only.projected_new_chunk_bytes, bytes.len() as u64);
            assert_eq!(
                crate::estimate::calculate_for_workspace(&root, &b.store, "t-unattached")
                    .unwrap()
                    .projected_new_chunk_bytes,
                selected_b.projected_new_chunk_bytes
            );
            let mut stream = ChunkStream::for_path(bytes.as_slice(), &path);
            let mut missing = BTreeMap::new();
            while let Some(chunk) = stream.next_chunk().unwrap() {
                let id = crate::store::object_id(ObjectKind::Chunk, &chunk);
                if !b.store.contains_object(&id).unwrap() {
                    missing.insert(id, chunk.len() as u64);
                }
            }
            assert_eq!(
                selected_b.projected_new_chunk_bytes,
                missing.values().sum::<u64>()
            );
            let captured = b.seal(None, SnapshotTrigger::Manual, None).unwrap();
            assert_ne!(
                entry(&b, &captured, b"transcript.jsonl").target,
                entry(&a, &original, b"transcript.jsonl").target
            );
            assert!(missing
                .keys()
                .all(|id| b.store.contains_object(id).unwrap()));
        });
    }
}
