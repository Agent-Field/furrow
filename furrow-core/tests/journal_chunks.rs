use furrow::chunker::MAX_CHUNK;
use furrow::estimate;
use furrow::exchange::export::{published, ExportJob, DEFAULT_MAX_FRAME};
use furrow::exchange::ledger::Ledger;
use furrow::exchange::materialize::materialize;
use furrow::exchange::open_store;
use furrow::model::{Blob, ObjectId, ObjectKind, Snapshot, SnapshotTrigger};
use furrow::sealer::Keys;
use furrow::tree;
use furrow::FurrowRepository;
use std::fs;
use std::io::Write;

fn journal_profile_on(test: &str) -> bool {
    if std::env::var("FURROW_JOURNAL_CHUNKS").is_ok_and(|value| value == "1") {
        return true;
    }
    // A child process enables the profile without changing other tests' environment.
    assert!(std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test])
        .env("FURROW_JOURNAL_CHUNKS", "1")
        .status()
        .unwrap()
        .success());
    false
}

fn journal_line(turn: usize, state: &mut u64) -> Vec<u8> {
    let size = (5 + (turn * 17) % 36) * 1024;
    let words = [
        "snapshot",
        "workspace",
        "journal",
        "command",
        "response",
        "storage",
        "capture",
        "file",
        "content",
        "message",
        "request",
        "result",
        "data",
        "path",
        "turn",
        "object",
    ];
    let mut content = String::new();
    while content.len() < size - 128 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        content.push_str(words[(*state >> 33) as usize % words.len()]);
        content.push(' ');
    }
    let mut raw = serde_json::to_vec(&serde_json::json!({
        "type": "message", "turn": turn, "content": content
    }))
    .unwrap();
    raw.push(b'\n');
    raw
}

fn seal(repo: &mut FurrowRepository) -> ObjectId {
    repo.seal(None, SnapshotTrigger::Manual, None).unwrap()
}

fn blob(repo: &FurrowRepository, head: ObjectId, path: &[u8]) -> Blob {
    let store = open_store(repo.store_root().parent().unwrap()).unwrap();
    let snapshot: Snapshot = store.read_struct(&head, ObjectKind::Snapshot).unwrap();
    let entry = tree::find_path(&store, &snapshot.root_tree, path)
        .unwrap()
        .unwrap();
    store
        .read_struct(&entry.target.unwrap(), ObjectKind::Blob)
        .unwrap()
}

#[test]
fn journal_j1_exported_frames_stay_below_three_times_appends() {
    if !journal_profile_on("journal_j1_exported_frames_stay_below_three_times_appends") {
        return;
    }
    for seed_bytes in [0, 8 * 1024 * 1024] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        let data = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let path = root.join("transcript.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        let mut state = 20261104;
        let mut size = 0;
        for turn in 1.. {
            if size >= seed_bytes {
                break;
            }
            let raw = journal_line(turn, &mut state);
            file.write_all(&raw).unwrap();
            size += raw.len();
        }
        let mut repo =
            FurrowRepository::attach_timeline_in(&data, &root, Some("chat"), None).unwrap();
        let keys = Keys {
            cell_key: [0x11; 32],
            dedup: [0x22; 32],
        };
        let ledger = Ledger::named(&data, "1234567890abcdef").unwrap();
        let store = open_store(&data).unwrap();
        let outbox = temp.path().join("outbox");
        let mut stored = 0;
        let mut appended = 0;
        state = 20261105;
        for turn in 0..=50 {
            if turn > 0 {
                let raw = journal_line(turn, &mut state);
                file.write_all(&raw).unwrap();
                appended += raw.len();
            }
            let head = seal(&mut repo);
            let report = ExportJob {
                store: &store,
                keys: &keys,
                head,
                ledger: &ledger,
                outbox: &outbox,
                max_frame: DEFAULT_MAX_FRAME,
            }
            .run()
            .unwrap();
            let paths: Vec<_> = report.frames.iter().map(|f| f.path.clone()).collect();
            let frame_bytes: usize = paths.iter().map(|p| fs::read(p).unwrap().len()).sum();
            assert_eq!(frame_bytes, report.bytes);
            if turn > 0 {
                stored += frame_bytes;
            }
            published(&ledger, &paths).unwrap();
        }
        assert!(
            stored <= 3 * appended,
            "seed={seed_bytes}, stored={stored}, appended={appended}"
        );
    }
}

#[test]
fn journal_j3_independent_captures_ignore_append_history() {
    if !journal_profile_on("journal_j3_independent_captures_ignore_append_history") {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let mut final_blobs = Vec::new();
    let mut state = 42;
    let bytes: Vec<u8> = (1..=20)
        .flat_map(|turn| journal_line(turn, &mut state))
        .collect();
    for (index, name) in ["transcript.JSONL", "events.NdJsOn"].iter().enumerate() {
        let root = temp.path().join(format!("tree-{index}"));
        let data = temp.path().join(format!("data-{index}"));
        fs::create_dir(&root).unwrap();
        let path = root.join(name);
        let mut repo =
            FurrowRepository::attach_timeline_in(&data, &root, Some("chat"), None).unwrap();
        if index == 0 {
            for prefix in [5000, 40_000, 120_000] {
                fs::write(&path, &bytes[..prefix]).unwrap();
                seal(&mut repo);
            }
        }
        fs::write(path, &bytes).unwrap();
        let head = seal(&mut repo);
        final_blobs.push(blob(&repo, head, name.as_bytes()));
    }
    assert_eq!(final_blobs[0], final_blobs[1]);
    assert!(final_blobs[0].chunks.len() > 3);
}

#[test]
fn journal_j4_existing_materializer_restores_bounded_chunks() {
    if !journal_profile_on("journal_j4_existing_materializer_restores_bounded_chunks") {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("tree");
    let data = temp.path().join("data");
    fs::create_dir(&root).unwrap();
    let mut state = 7;
    let mut bytes: Vec<u8> = (1..=100)
        .flat_map(|turn| journal_line(turn, &mut state))
        .collect();
    // An oversized unterminated record must still obey the reader's existing chunk limit.
    bytes.extend(std::iter::repeat_n(b'x', MAX_CHUNK * 2 + 17));
    fs::write(root.join("trajectory.ndjson"), &bytes).unwrap();
    let mut repo = FurrowRepository::attach_timeline_in(&data, &root, Some("chat"), None).unwrap();
    let head = seal(&mut repo);
    let captured = blob(&repo, head, b"trajectory.ndjson");
    assert_eq!(captured.total_len, bytes.len() as u64);
    assert!(captured.chunks.len() > 3);
    assert!(captured
        .chunks
        .iter()
        .all(|c| c.len > 0 && c.len as usize <= MAX_CHUNK));
    drop(repo);
    let target = temp.path().join("restored");
    fs::create_dir(&target).unwrap();
    materialize(&data, &target, None, head).unwrap();
    assert_eq!(fs::read(target.join("trajectory.ndjson")).unwrap(), bytes);
}

#[test]
fn journal_j5_estimate_uses_capture_selection() {
    if !journal_profile_on("journal_j5_estimate_uses_capture_selection") {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("tree");
    let data = temp.path().join("data");
    fs::create_dir_all(root.join("nested")).unwrap();
    let mut state = 19;
    let bytes: Vec<u8> = (1..=40)
        .flat_map(|turn| journal_line(turn, &mut state))
        .collect();
    for name in ["transcript.JsOnL", "nested/worker.NDJSON", "ordinary.json"] {
        fs::write(root.join(name), &bytes).unwrap();
    }
    let store = open_store(&data).unwrap();
    let before = estimate::calculate(&root, &store).unwrap();
    assert!(before.projected_new_chunk_bytes > 0);
    let mut repo = FurrowRepository::attach_timeline_in(&data, &root, Some("chat"), None).unwrap();
    seal(&mut repo);
    let after = estimate::calculate(&root, &store).unwrap();
    assert_eq!(after.projected_new_chunk_bytes, 0);
    assert_eq!(after.unique_chunks, before.unique_chunks);
    assert_eq!(
        after.deduplicated_chunk_bytes,
        before.projected_new_chunk_bytes
    );
}
