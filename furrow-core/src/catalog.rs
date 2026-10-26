use crate::model::{ObjectId, ObjectKind};
use anyhow::Context;
use rusqlite::config::DbConfig;
use rusqlite::{params, Connection, OptionalExtension};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ObjectLocation {
    pub pack: String,
    pub offset: u64,
    pub len: u64,
    pub kind: ObjectKind,
}

#[derive(Debug, Clone)]
pub struct PackCheckpoint {
    pub verified_len: u64,
    pub object_count: u64,
    pub last_object: Option<ObjectId>,
    pub last_record_start: u64,
}

/// Page cache for the catalog connection, in KiB.
const CACHE_KIB: i64 = 32 * 1024;
use crate::writer_wait::WRITER_WAIT;

fn writer_error(error: anyhow::Error) -> anyhow::Error {
    if error
        .downcast_ref::<rusqlite::Error>()
        .is_some_and(|error| {
            matches!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
            )
        })
    {
        anyhow::anyhow!(
            "store writer is still busy after waiting {} seconds; retry the operation",
            WRITER_WAIT.as_secs()
        )
    } else {
        error
    }
}

#[cfg(test)]
thread_local! {
    static WORK: Cell<u64> = const { Cell::new(0) };
}

/// SQLite virtual-machine steps this thread's catalogs have run, so a test can
/// pin how much work an operation does without timing it.
#[cfg(test)]
pub(crate) fn work_steps() -> u64 {
    WORK.with(Cell::get)
}

pub struct Catalog {
    conn: Connection,
    files: RefCell<HashMap<String, HashMap<Vec<u8>, CachedFile>>>,
    data_version: Cell<i64>,
    retain_files: Cell<bool>,
}

impl Catalog {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let _operation = crate::writer_wait::Operation::start();
        Self::open_inner(path).map_err(writer_error)
    }

    fn open_inner(path: &Path) -> anyhow::Result<Self> {
        let conn =
            Connection::open(path).with_context(|| format!("open catalog {}", path.display()))?;
        crate::writer_wait::configure(&conn)?;
        // Older writers must stop before binding a workspace to their obsolete
        // path index. The function name makes SQLite's refusal identify the format.
        conn.create_scalar_function(
            "requires_furrow_shared_paths_v1",
            0,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC
                | rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS,
            |_| Ok(1_i32),
        )?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // The catalog indexes the packs and the reference log, which are the
        // durable record: a commit lost to a crash is rebuilt at the next
        // open, so a commit does not sync. The log is still crash-consistent
        // (a lost suffix, never a torn database), and closing need not pay two
        // syncs to copy it back into the database file; the log is folded in
        // as it grows.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // A full walk looks up every file's stat row; a cache that holds the
        // hot pages spares each lookup a read from the file.
        conn.pragma_update(None, "cache_size", -CACHE_KIB)?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
        #[cfg(test)]
        conn.progress_handler(
            1,
            Some(|| {
                WORK.with(|work| work.set(work.get() + 1));
                false
            }),
        );
        crate::sqlite_adapter::fold_oversized_wal(&conn, path)?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS objects (
                id BLOB PRIMARY KEY,
                kind INTEGER NOT NULL,
                pack TEXT NOT NULL,
                offset INTEGER NOT NULL,
                len INTEGER NOT NULL
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS objects_pack_end ON objects(pack, (offset + len + 4));
            CREATE TABLE IF NOT EXISTS workspaces (
                id TEXT PRIMARY KEY,
                root BLOB NOT NULL UNIQUE,
                head BLOB
            );
            CREATE TRIGGER IF NOT EXISTS shared_paths_writer_version
                BEFORE INSERT ON workspaces BEGIN
                    SELECT requires_furrow_shared_paths_v1();
                END;
            CREATE TABLE IF NOT EXISTS timeline (
                workspace_id TEXT NOT NULL,
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                snapshot_id BLOB NOT NULL UNIQUE,
                sealed_at INTEGER NOT NULL,
                label TEXT,
                trigger TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS timeline_workspace_sequence
                ON timeline(workspace_id, sequence DESC);
            DROP TABLE IF EXISTS file_cache;
            CREATE TABLE IF NOT EXISTS stat_cache (
                workspace_id TEXT NOT NULL,
                path BLOB NOT NULL,
                device INTEGER NOT NULL,
                inode INTEGER NOT NULL,
                size INTEGER NOT NULL,
                mtime_secs INTEGER NOT NULL,
                mtime_nanos INTEGER NOT NULL,
                ctime_secs INTEGER NOT NULL,
                ctime_nanos INTEGER NOT NULL,
                mode INTEGER NOT NULL,
                blob_id BLOB NOT NULL,
                xattrs BLOB,
                sqlite INTEGER NOT NULL,
                cached_at_nanos INTEGER NOT NULL,
                PRIMARY KEY(workspace_id, path)
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS stat_cache_sqlite ON stat_cache(workspace_id, path) WHERE sqlite = 1;
            CREATE TABLE IF NOT EXISTS directory_cache (
                workspace_id TEXT NOT NULL,
                path BLOB NOT NULL,
                tree_id BLOB NOT NULL,
                PRIMARY KEY(workspace_id, path)
            ) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS pack_checkpoints (
                pack TEXT PRIMARY KEY,
                verified_len INTEGER NOT NULL,
                object_count INTEGER NOT NULL,
                last_object BLOB,
                last_record_start INTEGER NOT NULL
            ) WITHOUT ROWID;
            ",
        )?;
        Ok(Self {
            conn,
            files: Default::default(),
            data_version: Cell::new(-1),
            retain_files: Cell::new(false),
        })
    }

    pub(crate) fn forget_workspace_refs(&self, id: &str) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.conn
            .execute("DELETE FROM timeline WHERE workspace_id = ?1", [id])?;
        self.conn
            .execute("UPDATE workspaces SET head = NULL WHERE id = ?1", [id])?;
        Ok(())
    }

    pub(crate) fn forget_capture_caches(&self) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.files.borrow_mut().clear();
        self.conn
            .execute_batch("DELETE FROM stat_cache; DELETE FROM directory_cache;")?;
        Ok(())
    }

    pub(crate) fn defer_checkpoints(&self) -> anyhow::Result<()> {
        self.conn.pragma_update(None, "wal_autocheckpoint", 0)?;
        Ok(())
    }

    pub(crate) fn checkpoint(&self) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)")?;
        Ok(())
    }

    pub(crate) fn share_legacy_file_rows(
        &self,
        workspace: &str,
        folder: &str,
    ) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.files.borrow_mut().clear();
        let update = &UPSERT_STAT[UPSERT_STAT
            .find("ON CONFLICT")
            .expect("stat upsert has a conflict clause")..];
        self.conn.execute(
            &format!("INSERT INTO stat_cache SELECT ?2, path, device, inode, size, mtime_secs, mtime_nanos,
             ctime_secs, ctime_nanos, mode, blob_id, xattrs, sqlite, cached_at_nanos
             FROM stat_cache WHERE workspace_id = ?1 AND path != ?3 AND substr(path, 1, 6) != ?4
             {update} WHERE excluded.cached_at_nanos > stat_cache.cached_at_nanos"),
            params![workspace, folder, b".cell".as_slice(), b".cell/".as_slice()],
        )?;
        self.conn.execute(
            "DELETE FROM stat_cache WHERE workspace_id = ?1 AND path != ?2 AND substr(path, 1, 6) != ?3",
            params![workspace, b".cell".as_slice(), b".cell/".as_slice()],
        )?;
        Ok(())
    }

    pub fn cached_file(
        &self,
        workspace_id: &str,
        path: &[u8],
    ) -> anyhow::Result<Option<CachedFile>> {
        let _operation = crate::writer_wait::Operation::start();
        if !self.retain_files.get() {
            return self
                .conn
                .prepare_cached(SELECT_STAT)?
                .query_row(params![workspace_id, path], read_stat_row)
                .optional()?
                .map(CachedFile::try_from)
                .transpose();
        }
        if self.conn.is_autocommit() {
            self.refresh_file_cache()?;
        }
        if !self.files.borrow().contains_key(workspace_id) {
            let sql = SELECT_STAT.replace(
                "FROM stat_cache WHERE workspace_id = ?1 AND path = ?2",
                ", path FROM stat_cache WHERE workspace_id = ?1",
            );
            let mut statement = self.conn.prepare(&sql)?;
            let rows = statement.query_map([workspace_id], |row| {
                Ok((row.get::<_, Vec<u8>>(12)?, read_stat_row(row)?))
            })?;
            let mut files = HashMap::new();
            for row in rows {
                let (path, row) = row?;
                files.insert(path, CachedFile::try_from(row)?);
            }
            self.files
                .borrow_mut()
                .insert(workspace_id.to_owned(), files);
        }
        Ok(self
            .files
            .borrow()
            .get(workspace_id)
            .and_then(|files| files.get(path))
            .cloned())
    }

    /// Only retained daemon sessions keep folder-sized caches; one-shot walks retain bounded memory.
    pub(crate) fn retain_file_cache(&self) {
        self.retain_files.set(true);
    }

    /// An external commit invalidates retained rows before the next operation's snapshot is read.
    fn refresh_file_cache(&self) -> anyhow::Result<()> {
        if !self.retain_files.get() {
            return Ok(());
        }
        let version = self
            .conn
            .pragma_query_value(None, "data_version", |row| row.get(0))?;
        if self.data_version.replace(version) != version {
            self.files.borrow_mut().clear();
        }
        Ok(())
    }

    pub fn cached_files(
        &self,
        workspace_id: &str,
        paths: &[Vec<u8>],
    ) -> anyhow::Result<Vec<Option<CachedFile>>> {
        let _operation = crate::writer_wait::Operation::start();
        paths
            .iter()
            .map(|path| self.cached_file(workspace_id, path))
            .collect()
    }

    pub fn cache_file(
        &self,
        workspace_id: &str,
        path: &[u8],
        file: &CachedFile,
    ) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        if self.conn.is_autocommit() {
            self.refresh_file_cache()?;
        }
        if self
            .files
            .borrow()
            .get(workspace_id)
            .and_then(|rows| rows.get(path))
            == Some(file)
        {
            return Ok(());
        }
        self.conn.prepare_cached(UPSERT_STAT)?.execute(params![
            workspace_id,
            path,
            file.device as i64,
            crate::platform::sql_file_id(file.inode),
            file.size as i64,
            file.mtime_secs,
            file.mtime_nanos,
            file.ctime_secs,
            file.ctime_nanos,
            file.mode,
            file.blob_id.as_slice(),
            file.xattrs.as_ref().map(|id| id.as_slice()),
            file.sqlite,
            file.cached_at_nanos,
        ])?;
        if let Some(rows) = self.files.borrow_mut().get_mut(workspace_id) {
            rows.insert(path.to_vec(), file.clone());
        }
        Ok(())
    }

    /// Paths whose cached content is a SQLite database, in path order.
    pub fn sqlite_paths(&self, workspace_id: &str) -> anyhow::Result<Vec<Vec<u8>>> {
        let _operation = crate::writer_wait::Operation::start();
        let mut statement = self.conn.prepare_cached(
            "SELECT path FROM stat_cache WHERE workspace_id = ?1 AND sqlite = 1 ORDER BY path",
        )?;
        let rows = statement.query_map(params![workspace_id], |row| row.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Runs `read` against one snapshot of the catalog without taking its
    /// write lock, so several queries agree with each other while a writer
    /// commits between them.
    pub fn snapshot<T>(&self, read: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        if !self.conn.is_autocommit() {
            return read();
        }
        self.conn.execute_batch("BEGIN")?;
        let value = read();
        let _ = self.conn.execute_batch("ROLLBACK");
        value
    }

    /// Runs `write` as one transaction, so its many small catalog writes cost
    /// one durable commit instead of one each. Inside an open transaction it
    /// simply runs, and the outer commit covers it.
    pub fn batch<T>(&self, write: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        let _operation = crate::writer_wait::Operation::start();
        if !self.conn.is_autocommit() {
            return write();
        }
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|error| writer_error(error.into()))?;
        self.refresh_file_cache()?;
        match write() {
            Ok(value) => {
                if let Err(error) = self.conn.execute_batch("COMMIT") {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    self.files.borrow_mut().clear();
                    return Err(error.into());
                }
                Ok(value)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                self.files.borrow_mut().clear();
                Err(error)
            }
        }
    }

    pub fn cached_directory(
        &self,
        workspace_id: &str,
        path: &[u8],
    ) -> anyhow::Result<Option<ObjectId>> {
        let _operation = crate::writer_wait::Operation::start();
        let value: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT tree_id FROM directory_cache WHERE workspace_id = ?1 AND path = ?2",
                params![workspace_id, path],
                |row| row.get(0),
            )
            .optional()?;
        value.map(vec_to_id).transpose()
    }

    pub fn cache_directory(
        &self,
        workspace_id: &str,
        path: &[u8],
        tree_id: &ObjectId,
    ) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.conn.execute(
            "INSERT INTO directory_cache(workspace_id, path, tree_id) VALUES(?1, ?2, ?3)
             ON CONFLICT(workspace_id, path) DO UPDATE SET tree_id=excluded.tree_id",
            params![workspace_id, path, tree_id.as_slice()],
        )?;
        Ok(())
    }

    pub fn object(&self, id: &ObjectId) -> anyhow::Result<Option<ObjectLocation>> {
        let _operation = crate::writer_wait::Operation::start();
        self.conn
            .prepare_cached("SELECT kind, pack, offset, len FROM objects WHERE id = ?1")?
            .query_row(params![id.as_slice()], |row| {
                let kind_value: u8 = row.get(0)?;
                let kind = ObjectKind::from_u8(kind_value).ok_or_else(|| {
                    rusqlite::Error::InvalidColumnType(
                        0,
                        "kind".into(),
                        rusqlite::types::Type::Integer,
                    )
                })?;
                Ok(ObjectLocation {
                    kind,
                    pack: row.get(1)?,
                    offset: row.get::<_, i64>(2)? as u64,
                    len: row.get::<_, i64>(3)? as u64,
                })
            })
            .optional()
            .map_err(Into::into)
    }

    pub fn object_count(&self) -> anyhow::Result<u64> {
        let _operation = crate::writer_wait::Operation::start();
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM objects", [], |row| {
                row.get::<_, i64>(0)
            })? as u64)
    }

    pub fn object_payload_bytes(&self) -> anyhow::Result<u64> {
        let _operation = crate::writer_wait::Operation::start();
        Ok(self
            .conn
            .query_row("SELECT COALESCE(SUM(len), 0) FROM objects", [], |row| {
                row.get::<_, i64>(0)
            })? as u64)
    }

    pub fn remove_workspace(&self, workspace_id: &str) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.files.borrow_mut().clear();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM stat_cache WHERE workspace_id = ?1",
            params![workspace_id],
        )?;
        tx.execute(
            "DELETE FROM directory_cache WHERE workspace_id = ?1",
            params![workspace_id],
        )?;
        tx.execute(
            "DELETE FROM timeline WHERE workspace_id = ?1",
            params![workspace_id],
        )?;
        tx.execute(
            "DELETE FROM workspaces WHERE id = ?1",
            params![workspace_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn detach_workspace(&self, workspace_id: &str) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.files.borrow_mut().clear();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM stat_cache WHERE workspace_id = ?1",
            params![workspace_id],
        )?;
        tx.execute(
            "DELETE FROM directory_cache WHERE workspace_id = ?1",
            params![workspace_id],
        )?;
        tx.execute(
            "DELETE FROM workspaces WHERE id = ?1",
            params![workspace_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn replace_objects_from_gc(
        &self,
        mark_database: &Path,
        pack: &str,
        checkpoint: &PackCheckpoint,
    ) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.files.borrow_mut().clear();
        self.conn.execute(
            "ATTACH DATABASE ?1 AS gcmark",
            params![mark_database.as_os_str().to_string_lossy().as_ref()],
        )?;
        let result = (|| {
            let tx = self.conn.unchecked_transaction()?;
            tx.execute("DELETE FROM objects", [])?;
            tx.execute(
                "INSERT INTO objects(id, kind, pack, offset, len)
                 SELECT id, kind, ?1, offset, len FROM gcmark.locations",
                params![pack],
            )?;
            tx.execute("DELETE FROM stat_cache WHERE blob_id NOT IN (SELECT id FROM objects) OR (xattrs IS NOT NULL AND xattrs NOT IN (SELECT id FROM objects))", [])?;
            tx.execute(
                "DELETE FROM directory_cache WHERE tree_id NOT IN (SELECT id FROM objects)",
                [],
            )?;
            tx.execute("DELETE FROM pack_checkpoints", [])?;
            tx.execute(
                "INSERT INTO pack_checkpoints(
                    pack, verified_len, object_count, last_object, last_record_start
                 ) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![
                    pack,
                    checkpoint.verified_len as i64,
                    checkpoint.object_count as i64,
                    checkpoint.last_object.as_ref().map(|id| id.as_slice()),
                    checkpoint.last_record_start as i64,
                ],
            )?;
            tx.commit()?;
            Ok::<_, anyhow::Error>(())
        })();
        let detached = self.conn.execute("DETACH DATABASE gcmark", []);
        result?;
        detached?;
        Ok(())
    }

    pub fn pack_checkpoint(&self, pack: &str) -> anyhow::Result<Option<PackCheckpoint>> {
        let _operation = crate::writer_wait::Operation::start();
        self.conn
            .query_row(
                "SELECT verified_len, object_count, last_object, last_record_start
                 FROM pack_checkpoints WHERE pack = ?1",
                params![pack],
                |row| {
                    let last_object: Option<Vec<u8>> = row.get(2)?;
                    Ok((
                        row.get::<_, i64>(0)? as u64,
                        row.get::<_, i64>(1)? as u64,
                        last_object,
                        row.get::<_, i64>(3)? as u64,
                    ))
                },
            )
            .optional()?
            .map(
                |(verified_len, object_count, last_object, last_record_start)| {
                    Ok(PackCheckpoint {
                        verified_len,
                        object_count,
                        last_object: last_object.map(vec_to_id).transpose()?,
                        last_record_start,
                    })
                },
            )
            .transpose()
    }

    pub fn set_pack_checkpoint(
        &self,
        pack: &str,
        checkpoint: &PackCheckpoint,
    ) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.conn.execute(
            "INSERT INTO pack_checkpoints(
                pack, verified_len, object_count, last_object, last_record_start
             ) VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(pack) DO UPDATE SET
                verified_len=excluded.verified_len,
                object_count=excluded.object_count,
                last_object=excluded.last_object,
                last_record_start=excluded.last_record_start",
            params![
                pack,
                checkpoint.verified_len as i64,
                checkpoint.object_count as i64,
                checkpoint.last_object.as_ref().map(|id| id.as_slice()),
                checkpoint.last_record_start as i64,
            ],
        )?;
        Ok(())
    }

    /// The end of the furthest object row in `pack`: one seek down the
    /// `objects_pack_end` index, however many rows the pack holds.
    pub(crate) fn pack_high_water(&self, pack: &str) -> anyhow::Result<u64> {
        let _operation = crate::writer_wait::Operation::start();
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(offset + len + 4), 0) FROM objects WHERE pack = ?1",
            [pack],
            |row| row.get::<_, i64>(0),
        )? as u64)
    }

    pub fn reset_pack_index(&self, pack: &str) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.batch(|| {
            self.conn
                .execute("DELETE FROM objects WHERE pack = ?1", params![pack])?;
            self.conn.execute(
                "DELETE FROM pack_checkpoints WHERE pack = ?1",
                params![pack],
            )?;
            Ok(())
        })
    }

    pub fn insert_object(
        &self,
        id: &ObjectId,
        kind: ObjectKind,
        pack: &str,
        offset: u64,
        len: u64,
    ) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.conn.execute(
            "INSERT OR IGNORE INTO objects(id, kind, pack, offset, len) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![id.as_slice(), kind as u8, pack, offset as i64, len as i64],
        )?;
        Ok(())
    }

    pub fn delete_pack_objects_from(&self, pack: &str, payload_offset: u64) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.conn.execute(
            "DELETE FROM objects WHERE pack = ?1 AND offset >= ?2",
            params![pack, payload_offset as i64],
        )?;
        Ok(())
    }

    pub fn ensure_workspace(&self, id: &str, root: &[u8]) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.conn.execute(
            "INSERT OR IGNORE INTO workspaces(id, root) VALUES(?1, ?2)",
            params![id, root],
        )?;
        Ok(())
    }

    pub fn rebind_workspace(&self, id: &str, root: &[u8]) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.conn.execute(
            "UPDATE workspaces SET root = ?2 WHERE id = ?1",
            params![id, root],
        )?;
        Ok(())
    }

    pub fn workspace_head(&self, id: &str) -> anyhow::Result<Option<ObjectId>> {
        let _operation = crate::writer_wait::Operation::start();
        let value: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT head FROM workspaces WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        value.map(vec_to_id).transpose()
    }

    pub fn commit_snapshot(
        &self,
        workspace_id: &str,
        snapshot_id: &ObjectId,
        sealed_at: i64,
        label: Option<&str>,
        trigger: &str,
    ) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO timeline(workspace_id, snapshot_id, sealed_at, label, trigger)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                workspace_id,
                snapshot_id.as_slice(),
                sealed_at,
                label,
                trigger
            ],
        )?;
        tx.execute(
            "UPDATE workspaces SET head = ?2 WHERE id = ?1",
            params![workspace_id, snapshot_id.as_slice()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn timeline(&self, workspace_id: &str, limit: usize) -> anyhow::Result<Vec<TimelineRow>> {
        let _operation = crate::writer_wait::Operation::start();
        let mut stmt = self.conn.prepare(
            "SELECT snapshot_id, sealed_at, label, trigger FROM timeline
             WHERE workspace_id = ?1 ORDER BY sequence DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![workspace_id, limit as i64], |row| {
            let id: Vec<u8> = row.get(0)?;
            Ok((id, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (id, sealed_at, label, trigger) = row?;
            result.push(TimelineRow {
                id: vec_to_id(id)?,
                sealed_at,
                label,
                trigger,
            });
        }
        Ok(result)
    }
}

pub struct TimelineRow {
    pub id: ObjectId,
    pub sealed_at: i64,
    pub label: Option<String>,
    pub trigger: String,
}

/// One row of the stat cache: what a file looked like when it was last read,
/// and what reading it produced. A file whose stat still matches (and was not
/// touched within [`RACY_WINDOW_NANOS`] of being read) needs neither open nor
/// read nor xattr calls to be captured again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedFile {
    pub device: u64,
    pub inode: crate::platform::FileId,
    pub size: u64,
    pub mtime_secs: i64,
    pub mtime_nanos: i64,
    pub ctime_secs: i64,
    pub ctime_nanos: i64,
    pub mode: u32,
    pub blob_id: ObjectId,
    pub xattrs: Option<ObjectId>,
    pub sqlite: bool,
    /// Wall clock, in nanoseconds, just before the content was read.
    pub cached_at_nanos: i64,
}

/// Timestamps closer than this to the moment a file was read cannot prove the
/// content unchanged: a same-tick rewrite would leave them equal. Two seconds
/// covers the coarsest filesystem clocks in use.
pub const RACY_WINDOW_NANOS: i64 = 2_000_000_000;

impl CachedFile {
    /// True when a write in the timestamp granularity after the read could
    /// still have left the stat unchanged; such a file is read again.
    pub fn is_racy(&self) -> bool {
        #[cfg(unix)]
        let newest = self.mtime_nanos_total().max(self.ctime_nanos_total());
        #[cfg(windows)]
        let newest = self.mtime_nanos_total();
        newest.saturating_add(RACY_WINDOW_NANOS) >= self.cached_at_nanos
    }

    fn mtime_nanos_total(&self) -> i64 {
        self.mtime_secs
            .saturating_mul(1_000_000_000)
            .saturating_add(self.mtime_nanos)
    }

    #[cfg(unix)]
    fn ctime_nanos_total(&self) -> i64 {
        self.ctime_secs
            .saturating_mul(1_000_000_000)
            .saturating_add(self.ctime_nanos)
    }
}

const SELECT_STAT: &str = "SELECT device, inode, size, mtime_secs, mtime_nanos,
        ctime_secs, ctime_nanos, mode, blob_id, xattrs, sqlite, cached_at_nanos
     FROM stat_cache WHERE workspace_id = ?1 AND path = ?2";

const UPSERT_STAT: &str = "INSERT INTO stat_cache(
        workspace_id, path, device, inode, size, mtime_secs, mtime_nanos,
        ctime_secs, ctime_nanos, mode, blob_id, xattrs, sqlite, cached_at_nanos
     ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
     ON CONFLICT(workspace_id, path) DO UPDATE SET
        device=excluded.device, inode=excluded.inode, size=excluded.size,
        mtime_secs=excluded.mtime_secs, mtime_nanos=excluded.mtime_nanos,
        ctime_secs=excluded.ctime_secs, ctime_nanos=excluded.ctime_nanos,
        mode=excluded.mode, blob_id=excluded.blob_id, xattrs=excluded.xattrs,
        sqlite=excluded.sqlite, cached_at_nanos=excluded.cached_at_nanos";

/// A stat-cache row exactly as SQLite stores it.
struct StatRow {
    file: CachedFile,
    blob: Vec<u8>,
    xattrs: Option<Vec<u8>>,
}

fn read_stat_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StatRow> {
    Ok(StatRow {
        file: CachedFile {
            device: row.get::<_, i64>(0)? as u64,
            inode: crate::platform::read_file_id(row, 1)?,
            size: row.get::<_, i64>(2)? as u64,
            mtime_secs: row.get(3)?,
            mtime_nanos: row.get(4)?,
            ctime_secs: row.get(5)?,
            ctime_nanos: row.get(6)?,
            mode: row.get::<_, i64>(7)? as u32,
            blob_id: [0; 32],
            xattrs: None,
            sqlite: row.get(10)?,
            cached_at_nanos: row.get(11)?,
        },
        blob: row.get(8)?,
        xattrs: row.get(9)?,
    })
}

impl TryFrom<StatRow> for CachedFile {
    type Error = anyhow::Error;

    fn try_from(row: StatRow) -> anyhow::Result<Self> {
        let _operation = crate::writer_wait::Operation::start();
        Ok(CachedFile {
            blob_id: vec_to_id(row.blob)?,
            xattrs: row.xattrs.map(vec_to_id).transpose()?,
            ..row.file
        })
    }
}

fn vec_to_id(bytes: Vec<u8>) -> anyhow::Result<ObjectId> {
    anyhow::ensure!(bytes.len() == 32, "catalog contains an invalid object ID");
    let mut id = [0_u8; 32];
    id.copy_from_slice(&bytes);
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> (tempfile::TempDir, Catalog) {
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(&dir.path().join("catalog.sqlite3")).unwrap();
        (dir, catalog)
    }

    fn insert(catalog: &Catalog, byte: u8) -> anyhow::Result<()> {
        catalog.insert_object(&[byte; 32], ObjectKind::Chunk, "pack", 0, 1)
    }

    #[test]
    fn a_batch_commits_everything_or_nothing() {
        let (_dir, catalog) = catalog();
        let failed: anyhow::Result<()> = catalog.batch(|| {
            insert(&catalog, 1)?;
            anyhow::bail!("interrupted")
        });
        assert!(failed.is_err());
        assert!(catalog.object(&[1; 32]).unwrap().is_none());

        catalog
            .batch(|| {
                insert(&catalog, 2)?;
                insert(&catalog, 3)
            })
            .unwrap();
        assert!(catalog.object(&[2; 32]).unwrap().is_some());
        assert!(catalog.object(&[3; 32]).unwrap().is_some());
    }

    #[test]
    fn a_nested_batch_joins_the_outer_transaction() {
        let (_dir, catalog) = catalog();
        let outer: anyhow::Result<()> = catalog.batch(|| {
            catalog.batch(|| insert(&catalog, 4))?;
            anyhow::bail!("outer fails after the inner batch returned")
        });
        assert!(outer.is_err());
        assert!(catalog.object(&[4; 32]).unwrap().is_none());
    }

    #[test]
    fn a_racily_clean_row_is_told_apart_from_a_settled_one() {
        let row = |cached_at_nanos| CachedFile {
            device: 1,
            inode: 1,
            size: 1,
            mtime_secs: 100,
            mtime_nanos: 0,
            ctime_secs: 100,
            ctime_nanos: 0,
            mode: 0o100644,
            blob_id: [0; 32],
            xattrs: None,
            sqlite: false,
            cached_at_nanos,
        };
        assert!(row(100_500_000_000).is_racy());
        assert!(row(99_000_000_000).is_racy());
        assert!(!row(102_000_000_001).is_racy());
    }
}

#[cfg(test)]
mod q2_tests {
    use super::*;

    fn row() -> CachedFile {
        CachedFile {
            device: 1,
            inode: 2,
            size: 3,
            mtime_secs: 1,
            mtime_nanos: 0,
            ctime_secs: 1,
            ctime_nanos: 0,
            mode: 0o100644,
            blob_id: [1; 32],
            xattrs: None,
            sqlite: false,
            cached_at_nanos: 10_000_000_000,
        }
    }

    #[test]
    fn q2_unchanged_rows_do_not_write_and_external_commits_invalidate_memory() {
        // Q2/P4: retained folder rows never rewrite identical data and cannot hide another process's commit.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("catalog");
        let catalog = Catalog::open(&path).unwrap();
        catalog.retain_file_cache();
        let first = row();
        catalog.cache_file("folder", b"a", &first).unwrap();
        catalog
            .batch(|| {
                assert_eq!(catalog.cached_file("folder", b"a")?, Some(first.clone()));
                let changes = catalog.conn.total_changes();
                for _ in 0..10 {
                    catalog.cache_file("folder", b"a", &first)?;
                }
                assert_eq!(catalog.conn.total_changes(), changes);
                Ok(())
            })
            .unwrap();
        let other = Catalog::open(&path).unwrap();
        let mut second = first.clone();
        second.blob_id = [2; 32];
        other.cache_file("folder", b"a", &second).unwrap();
        catalog
            .batch(|| {
                assert_eq!(catalog.cached_file("folder", b"a")?, Some(second.clone()));
                Ok(())
            })
            .unwrap();
        let result: anyhow::Result<()> = catalog.batch(|| {
            catalog.cache_file("folder", b"a", &first)?;
            anyhow::bail!("abort capture")
        });
        assert!(result.is_err());
        assert_eq!(catalog.cached_file("folder", b"a").unwrap(), Some(second));
    }
}
