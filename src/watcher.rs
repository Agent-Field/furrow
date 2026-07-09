use crate::model::SnapshotTrigger;
use crate::repository::AgitRepository;
use anyhow::Context;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

const EVENT_QUEUE: usize = 4096;

pub fn run(mut repository: AgitRepository, debounce: Duration) -> anyhow::Result<()> {
    let (sender, receiver) = mpsc::sync_channel::<notify::Result<Event>>(EVENT_QUEUE);
    let overflow = Arc::new(AtomicBool::new(false));
    let callback_overflow = Arc::clone(&overflow);
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |event| {
        if sender.try_send(event).is_err() {
            callback_overflow.store(true, Ordering::Release);
        }
    })?;
    watcher.watch(repository.root(), RecursiveMode::Recursive)?;
    eprintln!(
        "agit: continuously protecting {} (Ctrl-C to stop)",
        repository.root().display()
    );

    loop {
        let first = receiver.recv().context("filesystem watcher stopped")?;
        if let Err(error) = first {
            eprintln!("agit: watcher warning: {error}");
            overflow.store(true, Ordering::Release);
        }

        loop {
            match receiver.recv_timeout(debounce) {
                Ok(Ok(_)) => continue,
                Ok(Err(error)) => {
                    eprintln!("agit: watcher warning: {error}");
                    overflow.store(true, Ordering::Release);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("filesystem watcher disconnected")
                }
            }
        }

        let label = if overflow.swap(false, Ordering::AcqRel) {
            "automatic snapshot after watcher overflow/rescan"
        } else {
            "automatic snapshot after write quiescence"
        };
        match repository.snapshot(Some(label.to_owned()), SnapshotTrigger::Watcher) {
            Ok(id) => eprintln!("agit: sealed {}", &hex::encode(id)[..12]),
            Err(error) => eprintln!("agit: automatic snapshot deferred: {error:#}"),
        }
    }
}
