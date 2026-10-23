//! Routes a request to its store's writer. Each store has one thread that owns
//! its open repository and runs that store's requests one at a time; requests
//! for different stores run in parallel on their own threads.

use crate::handle::Handle;
use crate::verbs::Verb;
use crate::wire::Target;
use anyhow::anyhow;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, sync_channel, RecvTimeoutError, Sender, SyncSender};
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc,
};
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const QUEUED: u8 = 0;
const RUNNING: u8 = 1;
const CANCELED: u8 = 2;

struct Job {
    verb: &'static dyn Verb,
    target: Target,
    args: Value,
    reply: Sender<anyhow::Result<Value>>,
    deadline: Instant,
    state: Arc<AtomicU8>,
}

struct Writer {
    jobs: Sender<Job>,
    thread: JoinHandle<anyhow::Result<()>>,
}

impl Writer {
    fn spawn(data_dir: PathBuf) -> Self {
        let (jobs, inbox) = channel::<Job>();
        let thread = std::thread::spawn(move || {
            let mut handles: HashMap<Target, Handle> = HashMap::new();
            let flusher = Flusher::start(furrow::repository::store_path(&data_dir));
            let mut deadline = Instant::now() + GROUP_INTERVAL;
            loop {
                match inbox.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(job) => {
                        let _operation = furrow::writer_wait::Operation::until(job.deadline);
                        if let Err(error) = furrow::writer_wait::check() {
                            let _ = job.reply.send(Err(error.into()));
                            continue;
                        }
                        if job
                            .state
                            .compare_exchange(QUEUED, RUNNING, Ordering::SeqCst, Ordering::SeqCst)
                            .is_err()
                        {
                            let _ = job.reply.send(Err(furrow::writer_wait::timeout().into()));
                            continue;
                        }
                        let error = flusher.error.lock().expect("flusher error").clone();
                        let result = match &error {
                            Some(error) => Err(anyhow!("group commit failed: {error}")),
                            None => {
                                if !handles.contains_key(&job.target) {
                                    let handle = match handles.values_mut().next() {
                                        Some(other) => Handle::sharing(job.target.clone(), other),
                                        None => Ok(Handle::new(job.target.clone())),
                                    };
                                    match handle {
                                        Ok(handle) => {
                                            handles.insert(job.target.clone(), handle);
                                        }
                                        Err(error) => {
                                            let _ = job.reply.send(Err(error));
                                            continue;
                                        }
                                    }
                                }
                                job.verb.run(
                                    handles.get_mut(&job.target).expect("inserted above"),
                                    job.args,
                                )
                            }
                        };
                        if handles.values().any(Handle::pending) {
                            flusher.dirty.store(true, Ordering::SeqCst);
                            for handle in handles.values_mut() {
                                handle.mark_clean();
                            }
                        }
                        let _ = job.reply.send(result);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        return flusher.finish();
                    }
                }
                if Instant::now() >= deadline {
                    deadline = Instant::now() + GROUP_INTERVAL;
                }
            }
        });
        Self { jobs, thread }
    }
}

// Opening the store on its writer keeps creation and earlier captures ordered before an external sync.
struct SyncBarrier;

impl Verb for SyncBarrier {
    fn name(&self) -> &'static str {
        "sync-barrier"
    }

    fn run(&self, handle: &mut Handle, _args: Value) -> anyhow::Result<Value> {
        handle.store()?;
        Ok(Value::Null)
    }
}

#[derive(Default)]
struct ActiveSyncs {
    count: Mutex<usize>,
    idle: Condvar,
}

impl ActiveSyncs {
    fn enter(&self) -> SyncGuard<'_> {
        *self.count.lock().expect("active syncs") += 1;
        SyncGuard(self)
    }

    fn wait(&self) {
        let mut count = self.count.lock().expect("active syncs");
        while *count != 0 {
            count = self.idle.wait(count).expect("active syncs");
        }
    }
}

struct SyncGuard<'a>(&'a ActiveSyncs);

impl Drop for SyncGuard<'_> {
    fn drop(&mut self) {
        *self.0.count.lock().expect("active syncs") -= 1;
        self.0.idle.notify_all();
    }
}

/// `None` once closed: no new writer starts and every request is refused.
pub struct Router {
    writers: Mutex<Option<HashMap<PathBuf, Writer>>>,
    syncs: ActiveSyncs,
}

impl Router {
    /// The fields start as "no writers yet", so the blank state is exactly
    /// `new()`; the attribute keeps that fact one lint away from the gate.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            writers: Mutex::new(Some(HashMap::new())),
            syncs: ActiveSyncs::default(),
        }
    }

    /// Runs the verb on the target's writer and waits for its answer.
    pub fn submit(
        &self,
        target: Target,
        verb: &'static dyn Verb,
        args: Value,
    ) -> anyhow::Result<Value> {
        self.submit_with_sync(target, verb, args, |root| {
            furrow::store::GroupCommit::open(root)?.sync()
        })
    }

    fn submit_with_sync(
        &self,
        target: Target,
        verb: &'static dyn Verb,
        args: Value,
        sync: impl FnOnce(&std::path::Path) -> anyhow::Result<()>,
    ) -> anyhow::Result<Value> {
        let _operation = furrow::writer_wait::Operation::start();
        let deadline = furrow::writer_wait::deadline();
        let syncing = verb.name() == "sync";
        // Register before writer admission so shutdown also joins work beyond its ordering barrier.
        let _sync = syncing.then(|| self.syncs.enter());
        let root = furrow::repository::store_path(&target.data_dir);
        let verb: &'static dyn Verb = if syncing { &SyncBarrier } else { verb };
        let (reply, answer) = channel();
        let state = Arc::new(AtomicU8::new(QUEUED));
        self.writer_inbox(&target)?
            .send(Job {
                verb,
                target,
                args,
                reply,
                deadline,
                state: Arc::clone(&state),
            })
            .map_err(|_| anyhow!("store writer stopped"))?;
        let result = match answer.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("store writer stopped")),
            Err(RecvTimeoutError::Timeout) => {
                if state
                    .compare_exchange(QUEUED, CANCELED, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    return Err(furrow::writer_wait::timeout().into());
                }
                // The original deadline still bounds every contended wait in the running job.
                // Its unblocked computation and storage I/O must finish without a false busy reply.
                answer.recv().map_err(|_| anyhow!("store writer stopped"))?
            }
        }?;
        if syncing {
            // The barrier includes preceding captures. Their device flush can run without occupying the writer.
            sync(&root)?;
            return Ok(json!({"durable": true}));
        }
        Ok(result)
    }

    fn writer_inbox(&self, target: &Target) -> anyhow::Result<Sender<Job>> {
        let mut guard = self.writers.lock().expect("router lock");
        let writers = guard
            .as_mut()
            .ok_or_else(|| anyhow!("daemon is stopping"))?;
        let writer = writers
            .entry(target.data_dir.clone())
            .or_insert_with(|| Writer::spawn(target.data_dir.clone()));
        Ok(writer.jobs.clone())
    }

    pub fn health(&self) -> Value {
        let stores = self
            .writers
            .lock()
            .expect("router lock")
            .as_ref()
            .map(HashMap::len);
        json!({"pid": std::process::id(), "stores": stores.unwrap_or(0)})
    }

    /// Refuses new work, lets every in-flight request finish, and drops every
    /// open repository.
    pub fn close(&self) {
        self.close_durable().expect("flush stores at shutdown");
    }

    pub(crate) fn close_durable(&self) -> anyhow::Result<()> {
        let writers = self.writers.lock().expect("router lock").take();
        self.syncs.wait();
        for (_, writer) in writers.unwrap_or_default() {
            drop(writer.jobs);
            writer
                .thread
                .join()
                .map_err(|_| anyhow!("store writer panicked"))??;
        }
        Ok(())
    }
}

const GROUP_INTERVAL: Duration = Duration::from_millis(250);

struct Flusher {
    dirty: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    stop: Sender<()>,
    thread: JoinHandle<anyhow::Result<()>>,
}

impl Flusher {
    fn start(root: PathBuf) -> Self {
        let checkpointer = Checkpointer::start(root.clone());
        Self::start_over(root, checkpointer)
    }

    fn start_over(root: PathBuf, checkpointer: Checkpointer) -> Self {
        let dirty = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let pending = Arc::clone(&dirty);
        let errors = Arc::clone(&error);
        let (stop, inbox) = channel();
        let thread = std::thread::spawn(move || {
            let mut group: Option<furrow::store::GroupCommit> = None;
            loop {
                let stopping = !matches!(
                    inbox.recv_timeout(GROUP_INTERVAL),
                    Err(RecvTimeoutError::Timeout)
                );
                if pending.swap(false, Ordering::SeqCst) {
                    let result = (|| {
                        let _operation = furrow::writer_wait::Operation::start();
                        if group.is_none() {
                            group = Some(furrow::store::GroupCommit::open(&root)?);
                        }
                        let group = group.as_mut().expect("opened above");
                        group.sync()?;
                        checkpointer.request();
                        Ok(())
                    })();
                    match result {
                        Ok(()) => {
                            *errors.lock().expect("flusher error") = None;
                        }
                        Err(error) => {
                            pending.store(true, Ordering::SeqCst);
                            *errors.lock().expect("flusher error") = Some(format!("{error:#}"));
                            eprintln!("group commit failed: {error:#}");
                            if stopping {
                                return Err(error);
                            }
                        }
                    }
                }
                if stopping {
                    return checkpointer.finish();
                }
            }
        });
        Self {
            dirty,
            error,
            stop,
            thread,
        }
    }

    fn finish(self) -> anyhow::Result<()> {
        let _ = self.stop.send(());
        self.thread
            .join()
            .map_err(|_| anyhow!("group flusher panicked"))?
    }
}

// Advisory folding must not delay the next durability tick or occupy the capture writer.
struct Checkpointer {
    requests: SyncSender<()>,
    thread: JoinHandle<anyhow::Result<()>>,
}

impl Checkpointer {
    fn start(root: PathBuf) -> Self {
        let mut group = None;
        Self::spawn(move || {
            let _operation = furrow::writer_wait::Operation::start();
            if group.is_none() {
                group = Some(furrow::store::GroupCommit::open(&root)?);
            }
            group.as_mut().expect("opened above").checkpoint_indexes()
        })
    }

    fn spawn(mut checkpoint: impl FnMut() -> anyhow::Result<()> + Send + 'static) -> Self {
        let (requests, inbox) = sync_channel(1);
        let thread = std::thread::spawn(move || {
            let mut last = Ok(());
            while inbox.recv().is_ok() {
                last = checkpoint();
                if let Err(error) = &last {
                    eprintln!("index checkpoint failed: {error:#}");
                }
            }
            last
        });
        Self { requests, thread }
    }

    fn request(&self) {
        // One queued request covers all outstanding index pages; a busy worker never stalls a flush.
        let _ = self.requests.try_send(());
    }

    fn finish(self) -> anyhow::Result<()> {
        drop(self.requests);
        self.thread
            .join()
            .map_err(|_| anyhow!("index checkpointer panicked"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p2_p5_sync_waits_for_durability_without_occupying_the_capture_writer() {
        // P2/P5/C28: slow device flushes leave the writer available, but shutdown and durable replies still join them.
        let temp = tempfile::tempdir().unwrap();
        let tree = temp.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("a"), b"completed capture").unwrap();
        let target = Target {
            data_dir: temp.path().join("data"),
            tree,
            cell_dir: None,
            timeline: Some("sync".into()),
        };
        let router = Arc::new(Router::new());
        router
            .submit(
                target.clone(),
                crate::verbs::lookup("seal").unwrap(),
                json!({}),
            )
            .unwrap();
        let root = furrow::repository::store_path(&target.data_dir);
        let workspace = furrow::repository::timeline_workspace_id("sync").unwrap();
        let expected = std::fs::metadata(root.join("workspaces").join(&workspace).join("refs.log"))
            .unwrap()
            .len();
        let (entered, ready) = channel();
        let (release, wait) = channel::<()>();
        let (finished, result) = channel();
        let r = Arc::clone(&router);
        let t = target.clone();
        let syncing = std::thread::spawn(move || {
            let answer = r.submit_with_sync(
                t,
                crate::verbs::lookup("sync").unwrap(),
                json!({}),
                |root| {
                    entered.send(()).unwrap();
                    wait.recv().unwrap();
                    furrow::store::GroupCommit::open(root)?.sync()
                },
            );
            finished.send(answer).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(
            result.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        let (planned, plan) = channel();
        let r = Arc::clone(&router);
        let planning = std::thread::spawn(move || {
            planned
                .send(r.submit(
                    target,
                    crate::verbs::lookup("capture-plan").unwrap(),
                    json!({}),
                ))
                .unwrap();
        });
        let answer = plan.recv_timeout(Duration::from_secs(10));
        let (closed, closing) = channel();
        let r = Arc::clone(&router);
        let shutdown = std::thread::spawn(move || closed.send(r.close_durable()).unwrap());
        let stopped_early = closing.recv_timeout(Duration::from_millis(100));
        // Always release the worker so an assertion failure cannot strand this fixture's shutdown.
        release.send(()).unwrap();
        assert!(
            matches!(stopped_early, Err(RecvTimeoutError::Timeout)),
            "shutdown interrupted an admitted sync"
        );
        let answer = answer.unwrap().unwrap();
        assert!(answer["token"].is_string());
        assert_eq!(
            result
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap()["durable"],
            true
        );
        planning.join().unwrap();
        syncing.join().unwrap();
        let frontier: Value =
            serde_json::from_slice(&std::fs::read(root.join("group-commit.json")).unwrap())
                .unwrap();
        assert_eq!(frontier["refs"][&workspace], expected);
        closing
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        shutdown.join().unwrap();
    }

    #[test]
    fn p5_slow_advisory_checkpoint_does_not_postpone_the_next_durable_group() {
        // P4/P5/C28: a blocked advisory checkpoint cannot keep later completed captures outside the durable frontier.
        let temp = tempfile::tempdir().unwrap();
        let tree = temp.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("a"), b"first").unwrap();
        let mut repo = furrow::FurrowRepository::attach_timeline_in(
            &temp.path().join("data"),
            &tree,
            Some("checkpoint"),
            None,
        )
        .unwrap();
        repo.defer_durability().unwrap();
        let (entered, ready) = channel();
        let (release, wait) = channel::<()>();
        let checkpoint = Checkpointer::spawn(move || {
            let _ = entered.send(());
            let _ = wait.recv();
            Ok(())
        });
        let root = repo.store_root().to_owned();
        let flusher = Flusher::start_over(root.clone(), checkpoint);
        repo.seal(None, furrow::model::SnapshotTrigger::AgentRun, None)
            .unwrap();
        flusher.dirty.store(true, Ordering::SeqCst);
        ready.recv_timeout(Duration::from_secs(10)).unwrap();
        std::fs::write(tree.join("a"), b"second").unwrap();
        repo.seal(None, furrow::model::SnapshotTrigger::AgentRun, None)
            .unwrap();
        let workspace = furrow::repository::timeline_workspace_id("checkpoint").unwrap();
        let expected = std::fs::metadata(root.join("workspaces").join(&workspace).join("refs.log"))
            .unwrap()
            .len();
        flusher.dirty.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let frontier: Value =
                serde_json::from_slice(&std::fs::read(root.join("group-commit.json")).unwrap())
                    .unwrap();
            if frontier["refs"][&workspace] == expected {
                break;
            }
            assert!(Instant::now() < deadline, "checkpoint delayed durability");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(release);
        flusher.finish().unwrap();
    }
}
