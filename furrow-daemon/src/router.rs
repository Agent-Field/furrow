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
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct Job {
    verb: &'static dyn Verb,
    target: Target,
    args: Value,
    reply: Sender<anyhow::Result<Value>>,
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

/// `None` once closed: no new writer starts and every request is refused.
pub struct Router {
    writers: Mutex<Option<HashMap<PathBuf, Writer>>>,
}

impl Router {
    /// The fields start as "no writers yet", so the blank state is exactly
    /// `new()`; the attribute keeps that fact one lint away from the gate.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            writers: Mutex::new(Some(HashMap::new())),
        }
    }

    /// Runs the verb on the target's writer and waits for its answer.
    pub fn submit(
        &self,
        target: Target,
        verb: &'static dyn Verb,
        args: Value,
    ) -> anyhow::Result<Value> {
        let (reply, answer) = channel();
        self.writer_inbox(&target)?
            .send(Job {
                verb,
                target,
                args,
                reply,
            })
            .map_err(|_| anyhow!("store writer stopped"))?;
        answer.recv().map_err(|_| anyhow!("store writer stopped"))?
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
                        if group.is_none() {
                            group = Some(furrow::store::GroupCommit::open(&root)?);
                        }
                        group.as_mut().expect("opened above").sync()
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
                    return Ok(());
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
