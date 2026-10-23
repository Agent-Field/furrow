//! One monotonic budget for all contended waits in an operation.

use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub const WRITER_WAIT: Duration = Duration::from_secs(30);
thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Nested operations inherit their caller's deadline, including on retained connections.
#[must_use]
pub struct Operation {
    previous: Option<Instant>,
    _thread: PhantomData<Rc<()>>,
}

impl Operation {
    pub fn start() -> Self {
        Self::until(Instant::now() + WRITER_WAIT)
    }

    /// Carries a request's original deadline across its writer queue.
    pub fn until(deadline: Instant) -> Self {
        let previous = DEADLINE
            .with(|slot| slot.replace(Some(slot.get().map_or(deadline, |old| old.min(deadline)))));
        Self {
            previous,
            _thread: PhantomData,
        }
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        DEADLINE.with(|slot| slot.set(self.previous));
    }
}

/// The current request deadline can be carried to another thread without restarting its budget.
pub fn deadline() -> Instant {
    DEADLINE.with(|slot| slot.get().unwrap_or_else(|| Instant::now() + WRITER_WAIT))
}

pub fn timeout() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "store writer is still busy after waiting {} seconds; retry the operation",
            WRITER_WAIT.as_secs()
        ),
    )
}

pub fn check() -> std::io::Result<()> {
    if DEADLINE.with(|slot| slot.get().is_some_and(|end| Instant::now() >= end)) {
        Err(timeout())
    } else {
        Ok(())
    }
}

/// Sleeps only within the current budget, even if a previous wait spent most of it.
pub fn pause(delay: Duration) -> std::io::Result<()> {
    check()?;
    let remaining = DEADLINE.with(|slot| {
        slot.get()
            .map(|end| end.saturating_duration_since(Instant::now()))
    });
    std::thread::sleep(remaining.map_or(delay, |remaining| delay.min(remaining)));
    check()
}

/// SQLite invokes this for every statement; the callback never renews the operation budget.
pub fn configure(connection: &rusqlite::Connection) -> rusqlite::Result<()> {
    connection.busy_handler(Some(|_| {
        DEADLINE.with(|slot| slot.get().is_some()) && pause(Duration::from_millis(10)).is_ok()
    }))
}

/// SQLite's convenience backup loop retries BUSY forever, so drive its steps within our budget.
pub(crate) fn backup(backup: &rusqlite::backup::Backup<'_, '_>, pages: i32) -> anyhow::Result<()> {
    let _operation = Operation::start();
    loop {
        match backup.step(pages)? {
            rusqlite::backup::StepResult::Done => return Ok(()),
            rusqlite::backup::StepResult::More => {}
            rusqlite::backup::StepResult::Busy | rusqlite::backup::StepResult::Locked => {
                pause(Duration::from_millis(2))?
            }
            _ => anyhow::bail!("unrecognized SQLite backup result"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2::FileExt;
    use std::fs::OpenOptions;

    #[test]
    fn m1_catalog_then_pack_lock_share_one_thirty_second_deadline() {
        // M1/S3: a successful catalog wait must leave only the unused budget for a different pack wait.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("store");
        let store = crate::store::ObjectStore::open(root.clone()).unwrap();
        let connection = rusqlite::Connection::open(root.join("catalog.sqlite3")).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        let pack = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("locks/pack.lock"))
            .unwrap();
        pack.lock_exclusive().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(8));
            connection.execute_batch("ROLLBACK").unwrap();
        });
        let start = Instant::now();
        let entered = Cell::new(false);
        let error = store
            .batched(|| {
                entered.set(true);
                store.put_bytes(
                    crate::model::ObjectKind::Chunk,
                    b"blocked after catalog wait",
                )
            })
            .unwrap_err();
        let elapsed = start.elapsed();
        release.join().unwrap();
        FileExt::unlock(&pack).unwrap();
        eprintln!("M1 catalog wait 8 s then pack contention: {elapsed:?}; {error:#}");
        assert!(
            entered.get(),
            "the first wait must succeed before the pack wait begins"
        );
        assert!(elapsed >= Duration::from_secs(29));
        assert!(elapsed < Duration::from_secs(35), "{elapsed:?}");
        assert!(error.to_string().contains("waiting 30 seconds"));
        assert!(error.to_string().contains("retry"));
        store
            .batched(|| store.put_bytes(crate::model::ObjectKind::Chunk, b"retry succeeds"))
            .unwrap();
    }

    #[test]
    fn m1_sqlite_backup_busy_steps_stop_at_the_operation_deadline() {
        // M1: SQLite backup must not retry a busy source forever or start another wait budget.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source");
        let owner = rusqlite::Connection::open(&path).unwrap();
        owner
            .execute_batch("CREATE TABLE data (value); BEGIN EXCLUSIVE")
            .unwrap();
        let source = rusqlite::Connection::open(&path).unwrap();
        configure(&source).unwrap();
        let mut destination = rusqlite::Connection::open_in_memory().unwrap();
        let copy = rusqlite::backup::Backup::new(&source, &mut destination).unwrap();
        let start = Instant::now();
        let _operation = Operation::until(start + Duration::from_millis(200));
        assert!(backup(&copy, 128).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn m1_repeated_sqlite_statements_and_nested_scopes_cannot_renew_a_deadline() {
        // M1: a retained connection and a fresh nested scope must both spend the original deadline.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let owner = rusqlite::Connection::open(&path).unwrap();
        owner
            .execute_batch("CREATE TABLE a (id); BEGIN IMMEDIATE")
            .unwrap();
        let retained = rusqlite::Connection::open(&path).unwrap();
        configure(&retained).unwrap();
        let start = Instant::now();
        {
            let _operation = Operation::until(start + Duration::from_millis(200));
            for _ in 0..3 {
                let _nested = Operation::start();
                assert!(retained.execute_batch("BEGIN IMMEDIATE").is_err());
            }
        }
        assert!(start.elapsed() < Duration::from_secs(1));
        owner.execute_batch("ROLLBACK").unwrap();
        // A subsequent operation on this retained connection gets its own budget.
        let _operation = Operation::start();
        retained.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    }
}
