//! M1 covers retained daemon sessions and time spent in the writer queue.

use furrow_daemon::{handle::Handle, router::Router, verbs::Verb, Target};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

static ENTERED: AtomicBool = AtomicBool::new(false);
struct Open;
impl Verb for Open {
    fn name(&self) -> &'static str {
        "open"
    }
    fn run(&self, handle: &mut Handle, _: Value) -> anyhow::Result<Value> {
        handle.store()?.defer_durability()?;
        Ok(json!({}))
    }
}
struct Write;
impl Verb for Write {
    fn name(&self) -> &'static str {
        "write"
    }
    fn run(&self, handle: &mut Handle, _: Value) -> anyhow::Result<Value> {
        let store = handle.store()?;
        store.batched(|| {
            ENTERED.store(true, Ordering::SeqCst);
            store.put_bytes(furrow::model::ObjectKind::Chunk, b"daemon write")?;
            Ok(json!({}))
        })
    }
}

#[test]
fn m1_retained_daemon_catalog_then_pack_wait_has_one_deadline() {
    // M1/S3/P2: retained sessions must not renew a request budget between catalog and pack contention.
    let temp = tempfile::tempdir().unwrap();
    let target = Target {
        data_dir: temp.path().join("data"),
        tree: temp.path().join("tree"),
        timeline: Some("chat".into()),
        cell_dir: None,
    };
    let router = Router::new();
    router.submit(target.clone(), &Open, json!({})).unwrap();
    let root = furrow::repository::store_path(&target.data_dir);
    let pack = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("locks/pack.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&pack).unwrap();
    let (ready, started) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        let catalog = furrow::catalog::Catalog::open(&root.join("catalog.sqlite3")).unwrap();
        catalog
            .batch(|| {
                ready.send(()).unwrap();
                std::thread::sleep(Duration::from_secs(8));
                Ok(())
            })
            .unwrap();
    });
    started.recv().unwrap();
    let start = Instant::now();
    let error = router
        .submit(target.clone(), &Write, json!({}))
        .unwrap_err();
    let elapsed = start.elapsed();
    holder.join().unwrap();
    fs2::FileExt::unlock(&pack).unwrap();
    assert!(ENTERED.load(Ordering::SeqCst));
    assert!(elapsed >= Duration::from_secs(29));
    assert!(elapsed < Duration::from_secs(35), "{elapsed:?}");
    assert!(error.to_string().contains("waiting 30 seconds"));
    eprintln!("M1 retained daemon: catalog 8 s, then pack, total {elapsed:?}; {error:#}");
    router.submit(target, &Write, json!({})).unwrap();
    router.close();
}

static SLOW_ENTERED: AtomicBool = AtomicBool::new(false);
static MUTATED: AtomicBool = AtomicBool::new(false);
struct Slow;
impl Verb for Slow {
    fn name(&self) -> &'static str {
        "slow"
    }
    fn run(&self, _: &mut Handle, _: Value) -> anyhow::Result<Value> {
        SLOW_ENTERED.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(500));
        Ok(json!({}))
    }
}
struct Mutate;
impl Verb for Mutate {
    fn name(&self) -> &'static str {
        "mutate"
    }
    fn run(&self, _: &mut Handle, _: Value) -> anyhow::Result<Value> {
        MUTATED.store(true, Ordering::SeqCst);
        Ok(json!({}))
    }
}

#[test]
fn m1_expired_queued_request_is_refused_before_its_verb_runs() {
    // M1: queue time is part of the request budget; an expired queued request cannot mutate later.
    let temp = tempfile::tempdir().unwrap();
    let target = Target {
        data_dir: temp.path().join("data"),
        tree: temp.path().join("tree"),
        timeline: Some("chat".into()),
        cell_dir: None,
    };
    let router = Arc::new(Router::new());
    let worker_router = Arc::clone(&router);
    let worker_target = target.clone();
    let worker = std::thread::spawn(move || {
        worker_router
            .submit(worker_target, &Slow, json!({}))
            .unwrap()
    });
    while !SLOW_ENTERED.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    {
        let _operation =
            furrow::writer_wait::Operation::until(Instant::now() + Duration::from_millis(100));
        assert!(router.submit(target.clone(), &Mutate, json!({})).is_err());
    }
    worker.join().unwrap();
    router.submit(target, &Open, json!({})).unwrap();
    assert!(!MUTATED.load(Ordering::SeqCst));
    router.close();
}

struct Complete;
impl Verb for Complete {
    fn name(&self) -> &'static str {
        "complete"
    }
    fn run(&self, _: &mut Handle, _: Value) -> anyhow::Result<Value> {
        std::thread::sleep(Duration::from_millis(250));
        Ok(json!({"complete": true}))
    }
}

#[test]
fn m1_p1_running_work_is_not_reported_as_writer_contention() {
    // M1/P1: a wait deadline bounds contention, not successful computation or a slow storage device.
    let temp = tempfile::tempdir().unwrap();
    let target = Target {
        data_dir: temp.path().join("data"),
        tree: temp.path().join("tree"),
        timeline: Some("chat".into()),
        cell_dir: None,
    };
    let router = Router::new();
    router.submit(target.clone(), &Open, json!({})).unwrap();
    {
        let _operation =
            furrow::writer_wait::Operation::until(Instant::now() + Duration::from_millis(100));
        assert_eq!(
            router.submit(target, &Complete, json!({})).unwrap(),
            json!({"complete": true})
        );
    }
    router.close();
}
