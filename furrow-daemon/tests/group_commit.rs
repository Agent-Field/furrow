//! P1/P2 exercise group durability independently of the request writer.

use furrow_daemon::{
    handle::Handle,
    router::Router,
    verbs::{self, Verb},
    Target,
};
use serde_json::{json, Value};
use std::fs;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};
use std::time::{Duration, Instant};

struct Block {
    entered: AtomicBool,
    released: Mutex<bool>,
    wake: Condvar,
}
static BLOCK: Block = Block {
    entered: AtomicBool::new(false),
    released: Mutex::new(false),
    wake: Condvar::new(),
};

impl Verb for Block {
    fn name(&self) -> &'static str {
        "block"
    }
    fn run(&self, handle: &mut Handle, _: Value) -> anyhow::Result<Value> {
        let store = handle.store()?.clone();
        store.batched(|| {
            store.put_bytes(
                furrow::model::ObjectKind::Chunk,
                b"an uncommitted capture record",
            )?;
            let mut released = self.released.lock().unwrap();
            self.entered.store(true, Ordering::SeqCst);
            while !*released {
                released = self.wake.wait(released).unwrap();
            }
            Ok(json!({}))
        })
    }
}
struct Release;
impl Drop for Release {
    fn drop(&mut self) {
        *BLOCK.released.lock().unwrap() = true;
        BLOCK.wake.notify_all();
    }
}

fn setup() -> (tempfile::TempDir, Target) {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    fs::create_dir(&tree).unwrap();
    fs::write(tree.join("a"), b"captured bytes").unwrap();
    let target = Target {
        tree,
        data_dir: dir.path().join("data"),
        cell_dir: None,
        timeline: Some("first".to_owned()),
    };
    (dir, target)
}

#[test]
fn p1_group_commit_does_not_wait_for_a_busy_capture_writer() {
    // P1/C28: a long request cannot keep an earlier completed capture beyond the group window.
    let (_dir, target) = setup();
    let router = Arc::new(Router::new());
    router
        .submit(target.clone(), verbs::lookup("seal").unwrap(), json!({}))
        .unwrap();
    let reference = target
        .data_dir
        .join("store-v1/workspaces")
        .join(furrow::repository::timeline_workspace_id("first").unwrap())
        .join("refs.log");
    let length = fs::metadata(&reference).unwrap().len();
    let worker_router = Arc::clone(&router);
    let worker_target = target.clone();
    let blocked = std::thread::spawn(move || {
        worker_router
            .submit(worker_target, &BLOCK, json!({}))
            .unwrap()
    });
    let release = Release;
    let start = Instant::now();
    while !BLOCK.entered.load(Ordering::SeqCst) {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    let workspace = furrow::repository::timeline_workspace_id("first").unwrap();
    loop {
        let frontier: Value = serde_json::from_slice(
            &fs::read(target.data_dir.join("store-v1/group-commit.json")).unwrap(),
        )
        .unwrap();
        if frontier["refs"][&workspace] == length {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "group commit was blocked by the writer"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let frontier: Value = serde_json::from_slice(
        &fs::read(target.data_dir.join("store-v1/group-commit.json")).unwrap(),
    )
    .unwrap();
    let actual = fs::metadata(target.data_dir.join("store-v1/packs/pack-000001.agp"))
        .unwrap()
        .len();
    assert!(
        frontier["packs"]["pack-000001.agp"].as_u64().unwrap() < actual,
        "the pending transaction must stay outside the durable frontier"
    );
    assert!(
        !blocked.is_finished(),
        "durability must complete while the request remains blocked"
    );
    drop(release);
    blocked.join().unwrap();
    router.close();
}

#[test]
fn p2_one_data_directory_keeps_one_store_session_for_many_timelines() {
    // P2/P4/S1: serving another chat in the same folder reuses one store session and preserves each head.
    let (_dir, first) = setup();
    let mut second = first.clone();
    second.timeline = Some("second".to_owned());
    let router = Router::new();
    let a = router
        .submit(first.clone(), verbs::lookup("seal").unwrap(), json!({}))
        .unwrap();
    let b = router
        .submit(second.clone(), verbs::lookup("seal").unwrap(), json!({}))
        .unwrap();
    assert_eq!(router.health()["stores"], 1);
    assert_ne!(a["snapshot"], b["snapshot"]);
    // P1: store-wide sync also works through a target that has no attached timeline or tree.
    let mut unattached = first.clone();
    unattached.timeline = Some("not-attached".to_owned());
    unattached.tree = first.tree.join("not-present");
    let synced = router
        .submit(unattached, verbs::lookup("sync").unwrap(), json!({}))
        .unwrap();
    assert_eq!(synced["durable"], true);
    fs::write(first.tree.join("a"), b"first chat changed").unwrap();
    router
        .submit(
            first,
            verbs::lookup("seal").unwrap(),
            json!({"changed":["a"]}),
        )
        .unwrap();
    let log = router
        .submit(second, verbs::lookup("log").unwrap(), json!({}))
        .unwrap();
    assert_eq!(log[0]["id"], b["snapshot"]);
    router.close();
}
