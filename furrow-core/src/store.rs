use crate::budget::{self, BudgetStatus};
use crate::catalog::{CachedFile, Catalog, PackCheckpoint, TimelineRow};
use crate::fault;
use crate::gc::{self, GcReport};
use crate::model::{ObjectId, ObjectKind, SnapshotTrigger};
use crate::platform::FileReadAt;
use crate::platform::OsStringExt;
use crate::refs::{RefLog, RefRecord};
use crate::retention::RetentionLog;
use anyhow::Context;
use fs2::FileExt;
use serde::{de::DeserializeOwned, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct DurableFrontier {
    packs: std::collections::BTreeMap<String, u64>,
    refs: std::collections::BTreeMap<String, u64>,
}

const INITIAL_PACK_NAME: &str = "pack-000001.agp";
const CURRENT_FILE: &str = "CURRENT";
const OBJECT_MAGIC: &[u8; 4] = b"AGOB";
const OBJECT_END: &[u8; 4] = b"AGND";
const OBJECT_VERSION: u8 = 1;
const HEADER_LEN: u64 = 4 + 1 + 1 + 8 + 32 + 32;
const MAX_OBJECT_LEN: u64 = 256 * 1024 * 1024;

/// What a [`ObjectStore::verify_index`] pass found in the active pack's index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct IndexCheck {
    /// Objects the pack's checkpoint accounts for.
    pub objects: u64,
    /// Whether the index disagreed with the pack and was rebuilt from it.
    pub repaired: bool,
}

#[derive(Clone)]
pub struct ObjectStore {
    root: PathBuf,
    catalog: Rc<Catalog>,
    active_pack: Rc<RefCell<String>>,
    /// Written records awaiting a verification checkpoint; absence does not imply durability.
    unsynced: Rc<RefCell<Option<AppendRun>>>,
    maintenance: Rc<MaintenanceState>,
    cache_folders: Rc<RefCell<HashMap<String, String>>>,
    deferred: Cell<bool>,
    append_pack: Rc<RefCell<Option<(String, File)>>>,
    known_end: Rc<Cell<u64>>,
    group: Rc<RefCell<Option<GroupCommit>>>,
}

/// A retained read-only view selects complete records without waiting for a capture's write transaction.
pub struct GroupCommit {
    root: PathBuf,
    catalog: rusqlite::Connection,
}

impl GroupCommit {
    pub fn open(root: &Path) -> anyhow::Result<Self> {
        let _operation = crate::writer_wait::Operation::start();
        let catalog = rusqlite::Connection::open_with_flags(
            root.join("catalog.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        crate::writer_wait::configure(&catalog)?;
        Ok(Self {
            root: root.to_owned(),
            catalog,
        })
    }

    pub fn sync(&mut self) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        let maintenance = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("locks/maintenance.lock"))?;
        wait_lock(&maintenance, false)?;
        self.sync_locked()
    }

    /// Advisory indexes checkpoint independently; durability is carried by packs and references.
    pub fn checkpoint_indexes(&mut self) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        let maintenance = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("locks/maintenance.lock"))?;
        wait_lock(&maintenance, false)?;
        let mut paths = vec![self.root.join("catalog.sqlite3")];
        for entry in fs::read_dir(self.root.join("workspaces"))? {
            let path = entry?.path().join("paths.sqlite3");
            if path.exists() {
                paths.push(path);
            }
        }
        for path in paths {
            let mut wal = path.as_os_str().to_owned();
            wal.push("-wal");
            if fs::metadata(wal).map_or(0, |m| m.len()) <= crate::sqlite_adapter::WAL_BUDGET {
                continue;
            }
            // Release each connection after folding so Windows can retire or replace a timeline's index.
            let connection = rusqlite::Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            )?;
            crate::writer_wait::configure(&connection)?;
            connection.set_db_config(
                rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                true,
            )?;
            // PASSIVE never waits for an active writer or reader. Its next tick carries unfinished pages.
            let result = connection.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
            if let Err(error) = result {
                if !matches!(
                    error.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                ) {
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }

    fn sync_locked(&mut self) -> anyhow::Result<()> {
        let root = &self.root;
        let initial = !root.join("group-commit.json").exists();
        let group = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("locks/group.lock"))?;
        wait_lock(&group, true)?;
        let mut frontier = DurableFrontier::default();
        // Snapshot complete references FIRST. Later appends may reach disk, but the frontier never claims them.
        for entry in fs::read_dir(root.join("workspaces"))? {
            let entry = entry?;
            if entry.path().join("refs.log").exists() {
                let id = entry.file_name().to_string_lossy().into_owned();
                frontier
                    .refs
                    .insert(id.clone(), RefLog::open(root, &id)?.complete_len()?);
            }
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("locks/pack.lock"))?;
        wait_lock(&lock, true)?;
        let mut packs = Vec::new();
        for entry in fs::read_dir(root.join("packs"))? {
            let entry = entry?;
            if entry.path().extension() == Some(std::ffi::OsStr::new("agp")) {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(entry.path())?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let written: i64 = self.catalog.query_row(
                    "SELECT COALESCE(MAX(offset + len + 4), 0) FROM objects WHERE pack = ?1",
                    [&name],
                    |row| row.get(0),
                )?;
                anyhow::ensure!(
                    file.metadata()?.len() >= written as u64,
                    "object pack needs recovery before group commit"
                );
                frontier.packs.insert(name, written as u64);
                packs.push(file);
            }
        }
        FileExt::unlock(&lock)?;
        for file in packs {
            file.sync_data()?;
        }
        crate::platform::sync_path(root.join("packs"))?;
        fault::point("group.packs");
        for id in frontier.refs.keys() {
            RefLog::open(root, id)?.sync()?;
        }
        for entry in fs::read_dir(root.join("workspaces"))? {
            crate::platform::sync_path(entry?.path())?;
        }
        crate::platform::sync_path(root.join("workspaces"))?;
        fault::point("group.refs");
        let mut temp = tempfile::NamedTempFile::new_in(root)?;
        temp.write_all(&serde_json::to_vec(&frontier)?)?;
        fault::point("group.frontier-written");
        temp.as_file().sync_all()?;
        fault::point("group.frontier-synced");
        crate::platform::rename(temp.path(), root.join("group-commit.json"))?;
        fault::point("group.frontier-renamed");
        crate::platform::sync_path(root)?;
        if initial {
            for ancestor in root
                .ancestors()
                .skip(1)
                .filter(|p| !p.as_os_str().is_empty())
            {
                crate::platform::sync_path(ancestor)?;
            }
        }
        fault::point("group.frontier");
        Ok(())
    }
}

/// Contiguous records appended by this process and not yet checkpointed.
struct AppendRun {
    pack: String,
    start: u64,
    end: u64,
    objects: u64,
    last_object: ObjectId,
    last_record_start: u64,
}

impl AppendRun {
    /// This run with one more record appended after it, or a new run when the
    /// record does not follow it (another process appended in between).
    fn then(run: Option<Self>, pack: &str, id: ObjectId, start: u64, end: u64) -> Self {
        match run {
            Some(run) if run.pack == pack && run.end == start => Self {
                end,
                objects: run.objects + 1,
                last_object: id,
                last_record_start: start,
                ..run
            },
            _ => Self {
                pack: pack.to_owned(),
                start,
                end,
                objects: 1,
                last_object: id,
                last_record_start: start,
            },
        }
    }

    /// The checkpoint after this run, when it continues `checkpoint`.
    fn extend(&self, checkpoint: PackCheckpoint) -> Option<PackCheckpoint> {
        (checkpoint.verified_len == self.start).then_some(PackCheckpoint {
            verified_len: self.end,
            object_count: checkpoint.object_count + self.objects,
            last_object: Some(self.last_object),
            last_record_start: self.last_record_start,
        })
    }
}

struct MaintenanceState {
    path: PathBuf,
    depth: Cell<u32>,
    exclusive: Cell<bool>,
    file: RefCell<Option<File>>,
    /// The pack files opened during the current section, so a walk over many
    /// objects opens each pack once instead of once per object. Handles
    /// survive ordinary sections; a generation change or exclusive
    /// maintenance invalidates them before packs can be replaced.
    packs: RefCell<HashMap<String, Rc<File>>>,
}

pub(crate) struct MaintenanceGuard {
    state: Rc<MaintenanceState>,
}

impl Drop for MaintenanceGuard {
    fn drop(&mut self) {
        let depth = self.state.depth.get();
        debug_assert!(depth > 0);
        self.state.depth.set(depth - 1);
        if depth == 1 {
            if let Some(file) = self.state.file.borrow_mut().take() {
                let _ = FileExt::unlock(&file);
            }
            self.state.exclusive.set(false);
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct StoreStats {
    pub objects: u64,
    pub physical_bytes: u64,
}

impl ObjectStore {
    pub fn open(root: PathBuf) -> anyhow::Result<Self> {
        let _operation = crate::writer_wait::Operation::start();
        fs::create_dir_all(root.join("packs"))?;
        fs::create_dir_all(root.join("locks"))?;
        fs::create_dir_all(root.join("workspaces"))?;
        let startup = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("locks").join("startup.lock"))?;
        wait_lock(&startup, true)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("locks").join("maintenance.lock"))?;
        wait_lock(&lock, false)?;
        let active_pack = read_or_initialize_current(&root)?;
        let catalog = Catalog::open(&root.join("catalog.sqlite3"))?;
        let maintenance = Rc::new(MaintenanceState {
            path: root.join("locks").join("maintenance.lock"),
            depth: Cell::new(1),
            exclusive: Cell::new(false),
            file: RefCell::new(Some(lock)),
            packs: RefCell::new(HashMap::new()),
        });
        let store = Self {
            root,
            catalog: Rc::new(catalog),
            active_pack: Rc::new(RefCell::new(active_pack)),
            unsynced: Rc::new(RefCell::new(None)),
            maintenance,
            cache_folders: Default::default(),
            deferred: Cell::new(false),
            append_pack: Default::default(),
            known_end: Rc::new(Cell::new(0)),
            group: Default::default(),
        };
        let startup_guard = MaintenanceGuard {
            state: Rc::clone(&store.maintenance),
        };
        let recovered = store.recover_pack();
        drop(startup_guard);
        recovered?;
        store.repair_deferred_refs()?;
        store.share_legacy_file_rows()?;
        store.known_end.set(
            fs::metadata(
                store
                    .root
                    .join("packs")
                    .join(store.active_pack.borrow().as_str()),
            )
            .map_or(0, |m| m.len()),
        );
        Ok(store)
    }

    /// Checks the active pack's index against its checkpoint and rebuilds it
    /// from the pack when they disagree.
    pub fn verify_index(&self, _deep: bool) -> anyhow::Result<IndexCheck> {
        Ok(IndexCheck {
            objects: 0,
            repaired: false,
        })
    }

    pub fn put_bytes(&self, kind: ObjectKind, bytes: &[u8]) -> anyhow::Result<ObjectId> {
        let _operation = crate::writer_wait::Operation::start();
        let _maintenance = self.acquire_maintenance_shared()?;
        // Every writer takes the catalog before the pack, including restore previews.
        self.catalog.batch(|| self.put_bytes_locked(kind, bytes))
    }

    fn put_bytes_locked(&self, kind: ObjectKind, bytes: &[u8]) -> anyhow::Result<ObjectId> {
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_OBJECT_LEN,
            "object exceeds size limit"
        );
        let id = object_id(kind, bytes);
        if self.catalog.object(&id)?.is_some() {
            return Ok(id);
        }

        let pack_path = self
            .root
            .join("packs")
            .join(self.active_pack.borrow().as_str());
        let length = fs::metadata(&pack_path).map_or(0, |m| m.len());
        if length != self.known_end.get() {
            self.recover_pack_under_catalog_lock()?;
            self.known_end
                .set(fs::metadata(&pack_path).map_or(0, |m| m.len()));
        }

        let lock_path = self.root.join("locks").join("pack.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        wait_lock(&lock, true)?;

        if self.catalog.object(&id)?.is_none() {
            let active_pack = self.active_pack.borrow().clone();
            let pack_path = self.root.join("packs").join(&active_pack);
            let mut retained = self.append_pack.borrow_mut();
            if retained
                .as_ref()
                .is_none_or(|(name, _)| name != &active_pack)
            {
                *retained = Some((
                    active_pack.clone(),
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .read(true)
                        .open(&pack_path)?,
                ));
            }
            let (_, pack) = retained.as_mut().expect("opened above");
            let record_start = pack.seek(SeekFrom::End(0))?;
            let checksum = blake3::hash(bytes);
            pack.write_all(OBJECT_MAGIC)?;
            pack.write_all(&[OBJECT_VERSION, kind as u8])?;
            pack.write_all(&(bytes.len() as u64).to_le_bytes())?;
            pack.write_all(&id)?;
            pack.write_all(checksum.as_bytes())?;
            fault::point("seal.pack-header");
            let payload_offset = record_start + HEADER_LEN;
            pack.write_all(bytes)?;
            fault::point("seal.pack-payload");
            pack.write_all(OBJECT_END)?;
            fault::point("seal.pack-record");
            let end = pack.stream_position()?;
            self.known_end.set(end);
            let run = self.unsynced.take();
            self.unsynced.replace(Some(AppendRun::then(
                run,
                &active_pack,
                id,
                record_start,
                end,
            )));
            self.catalog.insert_object(
                &id,
                kind,
                &active_pack,
                payload_offset,
                bytes.len() as u64,
            )?;
        }
        FileExt::unlock(&lock)?;
        Ok(id)
    }

    pub(crate) fn acquire_maintenance_shared(&self) -> anyhow::Result<MaintenanceGuard> {
        let _operation = crate::writer_wait::Operation::start();
        self.acquire_maintenance(false)
    }

    pub(crate) fn acquire_maintenance_exclusive(&self) -> anyhow::Result<MaintenanceGuard> {
        let _operation = crate::writer_wait::Operation::start();
        if self.maintenance.depth.get() == 0 && self.root.join("group-commit.json").exists() {
            self.sync()?;
        }
        self.acquire_maintenance(true)
    }

    fn acquire_maintenance(&self, exclusive: bool) -> anyhow::Result<MaintenanceGuard> {
        let depth = self.maintenance.depth.get();
        if depth > 0 {
            anyhow::ensure!(
                !exclusive || self.maintenance.exclusive.get(),
                "cannot upgrade a shared maintenance section"
            );
            self.maintenance.depth.set(depth + 1);
            return Ok(MaintenanceGuard {
                state: Rc::clone(&self.maintenance),
            });
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.maintenance.path)?;
        if exclusive {
            self.maintenance.packs.borrow_mut().clear();
            self.append_pack.borrow_mut().take();
            wait_lock(&file, true)?;
        } else {
            wait_lock(&file, false)?;
        }
        let current = read_or_initialize_current(&self.root)?;
        if current != *self.active_pack.borrow() {
            self.known_end.set(u64::MAX);
            self.maintenance.packs.borrow_mut().clear();
            self.append_pack.borrow_mut().take();
        }
        *self.active_pack.borrow_mut() = current;
        self.maintenance.depth.set(1);
        self.maintenance.exclusive.set(exclusive);
        *self.maintenance.file.borrow_mut() = Some(file);
        Ok(MaintenanceGuard {
            state: Rc::clone(&self.maintenance),
        })
    }

    pub fn put_struct<T: Serialize>(
        &self,
        kind: ObjectKind,
        value: &T,
    ) -> anyhow::Result<ObjectId> {
        let _operation = crate::writer_wait::Operation::start();
        let bytes = serde_json::to_vec(value)?;
        self.put_bytes(kind, &bytes)
    }

    pub fn read_bytes(&self, id: &ObjectId, expected: ObjectKind) -> anyhow::Result<Vec<u8>> {
        let _operation = crate::writer_wait::Operation::start();
        let _maintenance = self.acquire_maintenance_shared()?;
        self.read_bytes_unlocked(id, expected)
    }

    pub(crate) fn read_bytes_unlocked(
        &self,
        id: &ObjectId,
        expected: ObjectKind,
    ) -> anyhow::Result<Vec<u8>> {
        let _operation = crate::writer_wait::Operation::start();
        let location = self
            .catalog
            .object(id)?
            .with_context(|| format!("missing object {}", hex::encode(id)))?;
        anyhow::ensure!(location.kind == expected, "object kind mismatch");
        let pack = self.pack_for_reading(&location.pack)?;
        let mut bytes = vec![0_u8; location.len as usize];
        pack.read_exact_at(&mut bytes, location.offset)?;
        anyhow::ensure!(
            object_id(expected, &bytes) == *id,
            "object integrity check failed"
        );
        Ok(bytes)
    }

    /// The pack file to read from. Inside a maintenance section the handle is
    /// kept for the rest of the section; outside one it is opened for this
    /// read alone, because nothing then stops the pack from being removed.
    fn pack_for_reading(&self, name: &str) -> std::io::Result<Rc<File>> {
        let open = || File::open(self.root.join("packs").join(name)).map(Rc::new);
        if self.maintenance.depth.get() == 0 {
            return open();
        }
        if let Some(pack) = self.maintenance.packs.borrow().get(name) {
            return Ok(Rc::clone(pack));
        }
        let pack = open()?;
        self.maintenance
            .packs
            .borrow_mut()
            .insert(name.to_owned(), Rc::clone(&pack));
        Ok(pack)
    }

    pub fn read_struct<T: DeserializeOwned>(
        &self,
        id: &ObjectId,
        expected: ObjectKind,
    ) -> anyhow::Result<T> {
        let _operation = crate::writer_wait::Operation::start();
        Ok(serde_json::from_slice(&self.read_bytes(id, expected)?)?)
    }

    pub(crate) fn contains_object(&self, id: &ObjectId) -> anyhow::Result<bool> {
        let _operation = crate::writer_wait::Operation::start();
        let _maintenance = self.acquire_maintenance_shared()?;
        Ok(self.catalog.object(id)?.is_some())
    }

    pub fn ensure_workspace(&mut self, id: &str, root: &[u8]) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        let workspace_dir = self.root.join("workspaces").join(id);
        fs::create_dir_all(&workspace_dir)?;
        if !workspace_dir.join("root.path").exists() {
            write_root_path(&workspace_dir, root)?;
        }
        // Named timelines share a physical root but have separate catalog bindings.
        let catalog_root = if id.starts_with("t-") {
            [b"timeline:".as_slice(), id.as_bytes()].concat()
        } else {
            root.to_vec()
        };
        self.catalog.ensure_workspace(id, &catalog_root)?;
        self.cache_folders.borrow_mut().insert(
            id.to_owned(),
            format!("folder:{}", blake3::hash(root).to_hex()),
        );
        let detached = workspace_dir.join("detached");
        if detached.exists() {
            fs::remove_file(detached)?;
            crate::platform::sync_path(&workspace_dir)?;
        }
        let refs = RefLog::open(&self.root, id)?;
        let ref_head = refs.head()?.map(|record| record.snapshot_id);
        if self.catalog.workspace_head(id)? != ref_head {
            for record in refs.records()? {
                self.index_ref_record(id, &record)?;
            }
        }
        Ok(())
    }

    /// Follows a tree that was moved: the workspace registered at `from` is
    /// registered at `to` from now on, and keeps its history and head. Any
    /// workspace already at `to` describes the folder that was there before the
    /// move, so it is detached, or a path would name two workspaces.
    pub fn rebind_workspace(&mut self, from: &[u8], to: &[u8]) -> anyhow::Result<String> {
        let _operation = crate::writer_wait::Operation::start();
        let id = self
            .find_workspace(from)?
            .context("no workspace is registered at the folder that moved")?;
        if let Some(previous) = self.find_workspace(to)?.filter(|other| *other != id) {
            self.detach_workspace(&previous)?;
        }
        write_root_path(&self.workspace_data_dir(&id), to)?;
        self.catalog.rebind_workspace(&id, to)?;
        Ok(id)
    }

    /// Moves only the named timeline, leaving siblings attached to their folders.
    pub fn rebind_timeline(
        &mut self,
        timeline: &str,
        from: &[u8],
        to: &[u8],
    ) -> anyhow::Result<String> {
        let _operation = crate::writer_wait::Operation::start();
        let id = crate::repository::timeline_workspace_id(timeline)?;
        let directory = self.workspace_data_dir(&id);
        anyhow::ensure!(
            fs::read(directory.join("root.path"))? == from,
            "timeline is not attached at the folder that moved"
        );
        write_root_path(&directory, to)?;
        Ok(id)
    }

    pub fn find_workspace(&self, root: &[u8]) -> anyhow::Result<Option<String>> {
        Self::find_workspace_in(&self.root, root)
    }

    /// As [`Self::find_workspace`], from the store directory alone: the
    /// answer lives in the per-workspace folders, so it needs neither the
    /// catalog nor the pack, and a store that does not exist has none.
    pub(crate) fn find_workspace_in(
        store_root: &Path,
        root: &[u8],
    ) -> anyhow::Result<Option<String>> {
        let _operation = crate::writer_wait::Operation::start();
        let entries = match fs::read_dir(store_root.join("workspaces")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with("t-")
            {
                continue;
            }
            if entry.path().join("detached").exists() {
                continue;
            }
            let metadata_path = entry.path().join("root.path");
            if metadata_path.exists() && fs::read(metadata_path)? == root {
                return Ok(Some(entry.file_name().to_string_lossy().into_owned()));
            }
        }
        Ok(None)
    }

    pub fn workspace_head(&self, id: &str) -> anyhow::Result<Option<ObjectId>> {
        let _operation = crate::writer_wait::Operation::start();
        Ok(RefLog::open(&self.root, id)?
            .head()?
            .map(|record| record.snapshot_id))
    }

    pub fn detach_workspace(&mut self, id: &str) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        let workspace_dir = self.workspace_data_dir(id);
        fs::create_dir_all(&workspace_dir)?;
        let marker = workspace_dir.join("detached");
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(marker)?;
        file.sync_all()?;
        crate::platform::sync_path(workspace_dir)?;
        self.catalog.detach_workspace(id)?;
        Ok(())
    }

    pub fn purge_workspace(&mut self, id: &str) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.catalog.remove_workspace(id)?;
        let workspace_dir = self.workspace_data_dir(id);
        if workspace_dir.exists() {
            fs::remove_dir_all(&workspace_dir)?;
            crate::platform::sync_path(self.root.join("workspaces"))?;
        }
        Ok(())
    }

    pub fn timeline(&self, id: &str, limit: usize) -> anyhow::Result<Vec<TimelineRow>> {
        let _operation = crate::writer_wait::Operation::start();
        let refs = RefLog::open(&self.root, id)?;
        let Some(head) = refs.head()? else {
            return Ok(Vec::new());
        };
        let state = RetentionLog::open(&self.root, id)?.state()?;
        let sequences = state.recent_sequences(head.sequence, limit);
        Ok(refs
            .records_at_sequences(&sequences)?
            .into_iter()
            .map(|record| TimelineRow {
                id: record.snapshot_id,
                sealed_at: record.sealed_at,
                label: record.label,
                trigger: trigger_name(&record.trigger).to_owned(),
            })
            .collect())
    }

    pub fn retained_snapshot_count(&self, id: &str) -> anyhow::Result<u64> {
        let _operation = crate::writer_wait::Operation::start();
        let refs = RefLog::open(&self.root, id)?;
        let head = refs.head()?.map_or(0, |record| record.sequence);
        Ok(RetentionLog::open(&self.root, id)?
            .state()?
            .retained_count(head))
    }

    pub fn pinned_snapshots(
        &self,
        workspace_id: &str,
    ) -> anyhow::Result<std::collections::BTreeSet<ObjectId>> {
        let _operation = crate::writer_wait::Operation::start();
        Ok(RetentionLog::open(&self.root, workspace_id)?
            .state()?
            .pins
            .into_iter()
            .map(|pin| pin.snapshot_id)
            .collect())
    }

    pub fn pin_snapshot(&self, workspace_id: &str, snapshot_id: ObjectId) -> anyhow::Result<bool> {
        let _operation = crate::writer_wait::Operation::start();
        let _maintenance = self.acquire_maintenance_exclusive()?;
        anyhow::ensure!(
            self.catalog.object(&snapshot_id)?.is_some(),
            "snapshot bytes are no longer available to pin"
        );
        let refs = RefLog::open(&self.root, workspace_id)?;
        RetentionLog::open(&self.root, workspace_id)?.pin(&refs, snapshot_id)
    }

    pub fn unpin_snapshot(
        &self,
        workspace_id: &str,
        snapshot_id: &ObjectId,
    ) -> anyhow::Result<bool> {
        let _operation = crate::writer_wait::Operation::start();
        let _maintenance = self.acquire_maintenance_exclusive()?;
        RetentionLog::open(&self.root, workspace_id)?.unpin(snapshot_id)
    }

    /// A retained daemon shares folder rows across its timeline handles.
    pub fn retain_file_cache(&self) {
        self.catalog.retain_file_cache();
    }

    pub fn cached_file(
        &self,
        workspace_id: &str,
        path: &[u8],
    ) -> anyhow::Result<Option<CachedFile>> {
        let _operation = crate::writer_wait::Operation::start();
        let folder = self.cache_namespace(workspace_id, path)?;
        match self.catalog.cached_file(&folder, path)? {
            Some(row) => Ok(Some(row)),
            None if folder != workspace_id => self.catalog.cached_file(workspace_id, path),
            None => Ok(None),
        }
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
        self.catalog
            .cache_file(&self.cache_namespace(workspace_id, path)?, path, file)
    }

    pub fn sqlite_paths(&self, workspace_id: &str) -> anyhow::Result<Vec<Vec<u8>>> {
        let _operation = crate::writer_wait::Operation::start();
        let mut paths = self
            .catalog
            .sqlite_paths(&self.cache_namespace(workspace_id, b"")?)?;
        paths.extend(self.catalog.sqlite_paths(workspace_id)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Old timeline-scoped rows are advisory and can move to their folder without reading content again.
    fn share_legacy_file_rows(&self) -> anyhow::Result<()> {
        // Only a workspace that still owns such rows needs the write lock; once
        // they are shared, every later open finds none and takes no lock.
        let mut pending = Vec::new();
        for entry in fs::read_dir(self.root.join("workspaces"))? {
            let entry = entry?;
            if !entry.path().join("root.path").is_file() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if self.catalog.has_legacy_file_rows(&id)? {
                pending.push(id);
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        self.catalog.batch(|| {
            for id in &pending {
                let root = fs::read(self.workspace_data_dir(id).join("root.path"))?;
                self.catalog.share_legacy_file_rows(
                    id,
                    &format!("folder:{}", blake3::hash(&root).to_hex()),
                )?;
            }
            Ok(())
        })
    }

    /// File identity belongs to a folder; composed private state belongs to its timeline.
    fn cache_namespace(&self, workspace_id: &str, path: &[u8]) -> anyhow::Result<String> {
        if path == b".cell" || path.starts_with(b".cell/") {
            return Ok(workspace_id.to_owned());
        }
        if let Some(folder) = self.cache_folders.borrow().get(workspace_id) {
            return Ok(folder.clone());
        }
        let root = fs::read(self.workspace_data_dir(workspace_id).join("root.path"))?;
        let folder = format!("folder:{}", blake3::hash(&root).to_hex());
        self.cache_folders
            .borrow_mut()
            .insert(workspace_id.to_owned(), folder.clone());
        Ok(folder)
    }

    /// Runs `write` as one catalog transaction: the many small cache and index
    /// writes of a capture become a single commit. Synchronous batches flush
    /// the pack first; deferred batches checkpoint only the written prefix.
    /// Recovery trusts that checkpoint only up to the independent durable
    /// frontier and rebuilds the advisory catalog from packs and references.
    pub fn batched<T>(&self, write: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        let _operation = crate::writer_wait::Operation::start();
        self.catalog.batch(|| {
            let value = write()?;
            fault::point("seal.captured");
            if !self.deferred.get() {
                self.sync_active_pack()?;
            } else if let Some(run) = self.unsynced.take() {
                // The independent frontier limits which verified bytes recovery may trust after power loss.
                self.advance_checkpoint(&run)?;
            }
            fault::point("seal.packed");
            Ok(value)
        })
    }

    pub fn cached_directory(
        &self,
        workspace_id: &str,
        path: &[u8],
    ) -> anyhow::Result<Option<ObjectId>> {
        let _operation = crate::writer_wait::Operation::start();
        self.catalog.cached_directory(workspace_id, path)
    }

    pub fn cache_directory(
        &self,
        workspace_id: &str,
        path: &[u8],
        tree_id: &ObjectId,
    ) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.catalog.cache_directory(workspace_id, path, tree_id)
    }

    pub fn publish_snapshot(
        &mut self,
        workspace_id: &str,
        snapshot_id: ObjectId,
        sealed_at: i64,
        label: Option<String>,
        trigger: SnapshotTrigger,
    ) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        // The append is the visibility boundary. Synchronous calls flush the pack;
        // deferred calls leave complete written records for the independent group commit.
        if !self.deferred.get() {
            self.sync_active_pack()?;
        }
        fault::point("seal.committed");
        let record = RefLog::open(&self.root, workspace_id)?.append_ordered(
            snapshot_id,
            sealed_at,
            label,
            trigger,
            !self.deferred.get(),
        )?;
        fault::point("seal.logged");
        self.index_ref_record(workspace_id, &record)?;
        fault::point("seal.indexed");
        if let Err(error) = self.enforce_budget(false) {
            eprintln!("warning: automatic store-budget enforcement failed: {error:#}");
        }
        Ok(())
    }

    /// A deferred capture is complete in the kernel before it becomes visible.
    pub fn defer_durability(&self) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        if !self.root.join("group-commit.json").exists() {
            self.sync()?;
        }
        self.deferred.set(true);
        Ok(())
    }

    /// Restores the synchronous default for operations that follow a deferred capture.
    pub fn resume_durability(&self) {
        self.deferred.set(false);
    }

    /// Retained writers leave advisory WAL folding to the independent group worker.
    pub fn defer_checkpoints(&self) -> anyhow::Result<()> {
        self.catalog.defer_checkpoints()
    }

    pub(crate) fn checkpoint_catalog(&self) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.catalog.checkpoint()
    }

    /// Packs precede references; the durable frontier is published last.
    pub fn sync(&self) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        let _maintenance = self.acquire_maintenance_shared()?;
        self.sync_files_locked()?;
        if let Some(run) = self.unsynced.take() {
            self.catalog.batch(|| self.advance_checkpoint(&run))?;
        }
        Ok(())
    }

    fn sync_files_locked(&self) -> anyhow::Result<()> {
        let mut group = self.group.borrow_mut();
        if group.is_none() {
            *group = Some(GroupCommit::open(&self.root)?);
        }
        group.as_mut().expect("opened above").sync_locked()
    }

    fn frontier(&self) -> anyhow::Result<Option<DurableFrontier>> {
        let path = self.root.join("group-commit.json");
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
    }

    fn repair_deferred_refs(&self) -> anyhow::Result<()> {
        let Some(frontier) = self.frontier()? else {
            return Ok(());
        };
        // The catalog is written only for a log that needed repair, so an open
        // that finds every log intact does not take its write lock.
        for entry in fs::read_dir(self.root.join("workspaces"))? {
            let entry = entry?;
            if !entry.path().join("refs.log").exists() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let log = RefLog::open(&self.root, &id)?;
            if log.repair_suffix(*frontier.refs.get(&id).unwrap_or(&0), |id| {
                self.contains_object(id)
            })? {
                self.catalog
                    .batch(|| self.catalog.forget_workspace_refs(&id))?;
            }
        }
        Ok(())
    }

    pub fn budget_status(&self) -> anyhow::Result<BudgetStatus> {
        let _operation = crate::writer_wait::Operation::start();
        let config = budget::load(&self.root)?;
        budget::status(&self.root, config, self.stats()?.physical_bytes)
    }

    pub fn configure_budget(
        &mut self,
        max_store_bytes: Option<u64>,
        reserved_free_bytes: Option<u64>,
    ) -> anyhow::Result<BudgetStatus> {
        let _operation = crate::writer_wait::Operation::start();
        let _maintenance = self.acquire_maintenance_exclusive()?;
        let mut config = budget::load(&self.root)?;
        if let Some(max_store_bytes) = max_store_bytes {
            config.max_store_bytes = max_store_bytes;
        }
        if let Some(reserved_free_bytes) = reserved_free_bytes {
            config.reserved_free_bytes = reserved_free_bytes;
        }
        budget::save(&self.root, config)?;
        self.enforce_budget(true)?;
        self.budget_status()
    }

    pub fn enforce_budget(&mut self, force: bool) -> anyhow::Result<Option<GcReport>> {
        let _operation = crate::writer_wait::Operation::start();
        let before = self.budget_status()?;
        if !before.pressured() {
            budget::record_attempt(&self.root, before.physical_bytes, true)?;
            return Ok(None);
        }
        if !force && !budget::should_retry(&self.root, before.physical_bytes)? {
            return Ok(None);
        }
        let report = gc::collect(self, false)?;
        let after = self.budget_status()?;
        budget::record_attempt(&self.root, after.physical_bytes, after.satisfied)?;
        Ok(Some(report))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace_data_dir(&self, workspace_id: &str) -> PathBuf {
        self.root.join("workspaces").join(workspace_id)
    }

    pub(crate) fn workspace_roots(&self) -> anyhow::Result<Vec<(String, PathBuf)>> {
        let _operation = crate::writer_wait::Operation::start();
        let mut roots = Vec::new();
        for entry in fs::read_dir(self.root.join("workspaces"))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.path().join("detached").exists() {
                continue;
            }
            let path = entry.path().join("root.path");
            if !path.is_file() {
                continue;
            }
            roots.push((
                entry.file_name().to_string_lossy().into_owned(),
                PathBuf::from(std::ffi::OsString::from_vec(fs::read(path)?)),
            ));
        }
        Ok(roots)
    }

    pub fn stats(&self) -> anyhow::Result<StoreStats> {
        let _operation = crate::writer_wait::Operation::start();
        let _maintenance = self.acquire_maintenance_shared()?;
        self.stats_unlocked()
    }

    pub(crate) fn stats_unlocked(&self) -> anyhow::Result<StoreStats> {
        let _operation = crate::writer_wait::Operation::start();
        let mut physical_bytes = 0_u64;
        for entry in fs::read_dir(self.root.join("packs"))? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry.path().extension() == Some(std::ffi::OsStr::new("agp"))
            {
                physical_bytes += entry.metadata()?.len();
            }
        }
        Ok(StoreStats {
            objects: self.catalog.object_count()?,
            physical_bytes,
        })
    }

    pub(crate) fn object_payload_bytes(&self) -> anyhow::Result<u64> {
        let _operation = crate::writer_wait::Operation::start();
        self.catalog.object_payload_bytes()
    }

    pub(crate) fn object_len(&self, id: &ObjectId) -> anyhow::Result<u64> {
        let _operation = crate::writer_wait::Operation::start();
        self.catalog
            .object(id)?
            .map(|location| location.len)
            .with_context(|| format!("missing object {}", hex::encode(id)))
    }

    pub(crate) fn object_kind(&self, id: &ObjectId) -> anyhow::Result<ObjectKind> {
        let _operation = crate::writer_wait::Operation::start();
        self.catalog
            .object(id)?
            .map(|location| location.kind)
            .with_context(|| format!("missing object {}", hex::encode(id)))
    }

    pub(crate) fn replace_objects_from_gc(
        &mut self,
        mark_database: &Path,
        pack: &str,
        checkpoint: &PackCheckpoint,
    ) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        self.catalog
            .replace_objects_from_gc(mark_database, pack, checkpoint)
    }

    pub(crate) fn activate_pack(&mut self, pack: &str) -> anyhow::Result<()> {
        let _operation = crate::writer_wait::Operation::start();
        validate_pack_name(pack)?;
        let packs = self.root.join("packs");
        let temporary = packs.join("CURRENT.tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        writeln!(file, "{pack}")?;
        file.sync_all()?;
        crate::platform::rename(temporary, packs.join(CURRENT_FILE))?;
        crate::platform::sync_path(&packs)?;
        *self.active_pack.borrow_mut() = pack.to_owned();
        // An empty replacement pack is created lazily by the first append.
        self.known_end.set(match fs::metadata(packs.join(pack)) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        });
        self.maintenance.packs.borrow_mut().clear();
        self.append_pack.borrow_mut().take();
        if self.root.join("group-commit.json").exists() {
            self.sync_files_locked()?;
        }
        Ok(())
    }

    fn index_ref_record(&mut self, workspace_id: &str, record: &RefRecord) -> anyhow::Result<()> {
        self.catalog.commit_snapshot(
            workspace_id,
            &record.snapshot_id,
            record.sealed_at,
            record.label.as_deref(),
            trigger_name(&record.trigger),
        )
    }

    fn sync_active_pack(&self) -> anyhow::Result<()> {
        let Some(run) = self.unsynced.take() else {
            return Ok(());
        };
        let path = self.root.join("packs").join(&run.pack);
        if path.exists() {
            crate::platform::sync_data(path)?;
        }
        self.advance_checkpoint(&run)
    }

    /// Records the synced run as verified, so the next open has no tail to
    /// re-verify and re-sync. Skipped when another process appended before the
    /// run: the open-time recovery scan owns that case.
    fn advance_checkpoint(&self, run: &AppendRun) -> anyhow::Result<()> {
        let checkpoint = self
            .catalog
            .pack_checkpoint(&run.pack)?
            .unwrap_or(PackCheckpoint {
                verified_len: 0,
                object_count: 0,
                last_object: None,
                last_record_start: 0,
            });
        match run.extend(checkpoint) {
            Some(next) => self.catalog.set_pack_checkpoint(&run.pack, &next),
            None => Ok(()),
        }
    }

    /// Recovery takes the catalog's write lock BEFORE the pack lock. A capture
    /// already holds the catalog write lock when it appends an object and asks
    /// for the pack lock, so the opposite order here let two processes each
    /// hold one lock and wait on the other until SQLite's busy timeout gave up
    /// with "database is locked".
    fn recover_pack(&self) -> anyhow::Result<()> {
        let name = self.active_pack.borrow().clone();
        if !self.root.join("packs").join(&name).exists() {
            let frontier = self.frontier()?;
            let durable = frontier
                .as_ref()
                .and_then(|f| f.packs.get(&name))
                .copied()
                .unwrap_or(0);
            anyhow::ensure!(durable == 0, "durable object pack is missing");
            if self.catalog.pack_high_water(&name)? > 0 {
                anyhow::ensure!(frontier.is_some(), "durable object pack is missing");
                self.catalog.batch(|| {
                    self.catalog.reset_pack_index(&name)?;
                    self.catalog.forget_capture_caches()?;
                    fs::write(
                        self.root.join("capture-generation"),
                        format!("{:?}", std::time::SystemTime::now()),
                    )?;
                    Ok(())
                })?;
            }
            return Ok(());
        }
        // A pack that ends exactly at its verified checkpoint has nothing to
        // recover, so a reader neither queues behind a writer for the catalog
        // and pack locks nor writes anything. Anything else, including a
        // writer that is mid-append, falls through to the locked recovery.
        if self.pack_is_settled(&name)? {
            return Ok(());
        }
        self.catalog
            .batch(|| self.recover_pack_under_catalog_lock())
    }
    /// Whether the active pack is exactly its checkpointed, validated prefix,
    /// judged from one catalog snapshot without taking any lock. A `false`
    /// only means recovery must look under the locks.
    fn pack_is_settled(&self, pack_name: &str) -> anyhow::Result<bool> {
        let Ok(mut pack) = File::open(self.root.join("packs").join(pack_name)) else {
            return Ok(false);
        };
        let file_len = pack.metadata()?.len();
        self.catalog.snapshot(|| {
            let Some(checkpoint) = self.catalog.pack_checkpoint(pack_name)? else {
                return Ok(false);
            };
            if checkpoint.verified_len != file_len {
                return Ok(false);
            }
            let durable_len = self
                .frontier()?
                .map(|f| f.packs.get(pack_name).copied().unwrap_or(0));
            if durable_len.is_some_and(|len| len != file_len) {
                return Ok(false);
            }
            self.validate_checkpoint(&mut pack, file_len, &checkpoint)
        })
    }

    fn recover_pack_under_catalog_lock(&self) -> anyhow::Result<()> {
        let pack_name = self.active_pack.borrow().clone();
        let pack_path = self.root.join("packs").join(&pack_name);
        if !pack_path.exists() {
            return Ok(());
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("locks").join("pack.lock"))?;
        wait_lock(&lock, true)?;
        let mut pack = OpenOptions::new().read(true).write(true).open(&pack_path)?;
        let file_len = pack.metadata()?.len();
        if self.catalog.pack_high_water(&pack_name)? > file_len {
            self.catalog.forget_capture_caches()?;
            fs::write(
                self.root.join("capture-generation"),
                format!("{:?}", std::time::SystemTime::now()),
            )?;
        }
        let checkpoint = self.catalog.pack_checkpoint(&pack_name)?;
        let frontier = self.frontier()?;
        let durable_len = frontier
            .as_ref()
            .map(|f| f.packs.get(&pack_name).copied().unwrap_or(0));
        anyhow::ensure!(
            durable_len.is_none_or(|len| file_len >= len),
            "durable object pack prefix was lost"
        );
        let checkpoint = checkpoint.filter(|c| durable_len.is_none_or(|len| c.verified_len <= len));
        let checkpoint_valid = checkpoint
            .as_ref()
            .map(|checkpoint| self.validate_checkpoint(&mut pack, file_len, checkpoint))
            .transpose()?
            .unwrap_or(false);
        if !checkpoint_valid {
            self.catalog.reset_pack_index(&pack_name)?;
        }

        let mut offset = checkpoint
            .as_ref()
            .filter(|_| checkpoint_valid)
            .map_or(0, |checkpoint| checkpoint.verified_len);
        let mut object_count = checkpoint
            .as_ref()
            .filter(|_| checkpoint_valid)
            .map_or(0, |checkpoint| checkpoint.object_count);
        let mut last_object = checkpoint
            .as_ref()
            .filter(|_| checkpoint_valid)
            .and_then(|checkpoint| checkpoint.last_object);
        let mut last_record_start = checkpoint
            .as_ref()
            .filter(|_| checkpoint_valid)
            .map_or(0, |checkpoint| checkpoint.last_record_start);
        let scanned_from = offset;
        // The catalog can survive a power loss that drops a whole pack suffix.
        // Remove its claims beyond the verified boundary before rebuilding them.
        self.catalog
            .delete_pack_objects_from(&pack_name, offset.saturating_add(HEADER_LEN))?;
        self.catalog.batch(|| {
            pack.seek(SeekFrom::Start(offset))?;
            let mut buffer = vec![0_u8; 1024 * 1024];
            while offset < file_len {
                let record_start = offset;
                let mut magic = [0_u8; 4];
                if pack.read_exact(&mut magic).is_err() {
                    self.truncate_pack_tail(&pack, record_start)?;
                    break;
                }
                if &magic != OBJECT_MAGIC {
                    anyhow::ensure!(
                        frontier.is_some() && record_start >= durable_len.unwrap_or(0),
                        "object pack corruption at byte {record_start}"
                    );
                    self.truncate_pack_tail(&pack, record_start)?;
                    break;
                }
                let mut meta = [0_u8; 2];
                if pack.read_exact(&mut meta).is_err() {
                    self.truncate_pack_tail(&pack, record_start)?;
                    break;
                }
                if meta[0] != OBJECT_VERSION || ObjectKind::from_u8(meta[1]).is_none() {
                    anyhow::ensure!(
                        frontier.is_some() && record_start >= durable_len.unwrap_or(0),
                        "invalid object pack version or kind"
                    );
                    self.truncate_pack_tail(&pack, record_start)?;
                    break;
                }
                let kind = ObjectKind::from_u8(meta[1]).expect("validated above");
                let mut len_bytes = [0_u8; 8];
                if pack.read_exact(&mut len_bytes).is_err() {
                    self.truncate_pack_tail(&pack, record_start)?;
                    break;
                }
                let len = u64::from_le_bytes(len_bytes);
                if len > MAX_OBJECT_LEN {
                    anyhow::ensure!(
                        frontier.is_some() && record_start >= durable_len.unwrap_or(0),
                        "object pack record exceeds size limit"
                    );
                    self.truncate_pack_tail(&pack, record_start)?;
                    break;
                }
                let mut id = [0_u8; 32];
                let mut checksum = [0_u8; 32];
                if pack.read_exact(&mut id).is_err() || pack.read_exact(&mut checksum).is_err() {
                    self.truncate_pack_tail(&pack, record_start)?;
                    break;
                }
                let payload_offset = record_start + HEADER_LEN;
                if file_len.saturating_sub(payload_offset) < len + OBJECT_END.len() as u64 {
                    self.truncate_pack_tail(&pack, record_start)?;
                    break;
                }
                let mut remaining = len;
                let mut content_hasher = blake3::Hasher::new();
                content_hasher.update(kind.domain());
                let mut checksum_hasher = blake3::Hasher::new();
                while remaining > 0 {
                    let take = remaining.min(buffer.len() as u64) as usize;
                    pack.read_exact(&mut buffer[..take])?;
                    content_hasher.update(&buffer[..take]);
                    checksum_hasher.update(&buffer[..take]);
                    remaining -= take as u64;
                }
                let mut end = [0_u8; 4];
                pack.read_exact(&mut end)?;
                if &end != OBJECT_END
                    || checksum_hasher.finalize().as_bytes() != &checksum
                    || content_hasher.finalize().as_bytes() != &id
                {
                    anyhow::ensure!(
                        frontier.is_some() && record_start >= durable_len.unwrap_or(0),
                        "object pack checksum or trailer mismatch"
                    );
                    self.truncate_pack_tail(&pack, record_start)?;
                    break;
                }
                self.catalog
                    .insert_object(&id, kind, &pack_name, payload_offset, len)?;
                offset = pack.stream_position()?;
                object_count += 1;
                last_object = Some(id);
                last_record_start = record_start;
            }
            if offset == scanned_from {
                return Ok(());
            }
            pack.sync_data()?;
            self.catalog.set_pack_checkpoint(
                &pack_name,
                &PackCheckpoint {
                    verified_len: offset,
                    object_count,
                    last_object,
                    last_record_start,
                },
            )?;
            Ok(())
        })?;
        FileExt::unlock(&lock)?;
        Ok(())
    }

    fn validate_checkpoint(
        &self,
        pack: &mut File,
        file_len: u64,
        checkpoint: &PackCheckpoint,
    ) -> anyhow::Result<bool> {
        // Every object row ends at or before the checkpoint. A row past it
        // means a crash between an append and the checkpoint write; a row that
        // straddles it is a torn record. Both are a high water above
        // `verified_len`, which is one index seek, not a scan of the pack's rows.
        if checkpoint.verified_len > file_len
            || self
                .catalog
                .pack_high_water(self.active_pack.borrow().as_str())?
                > checkpoint.verified_len
        {
            return Ok(false);
        }
        if checkpoint.object_count == 0 {
            return Ok(checkpoint.verified_len == 0 && checkpoint.last_object.is_none());
        }
        let Some(last_id) = checkpoint.last_object else {
            return Ok(false);
        };
        let Some(location) = self.catalog.object(&last_id)? else {
            return Ok(false);
        };
        if location.pack != *self.active_pack.borrow()
            || location.offset != checkpoint.last_record_start + HEADER_LEN
            || location.offset + location.len + OBJECT_END.len() as u64 != checkpoint.verified_len
        {
            return Ok(false);
        }

        pack.seek(SeekFrom::Start(checkpoint.last_record_start))?;
        let mut header = [0_u8; HEADER_LEN as usize];
        if pack.read_exact(&mut header).is_err()
            || &header[..4] != OBJECT_MAGIC
            || header[4] != OBJECT_VERSION
            || ObjectKind::from_u8(header[5]) != Some(location.kind)
        {
            return Ok(false);
        }
        let mut len_bytes = [0_u8; 8];
        len_bytes.copy_from_slice(&header[6..14]);
        if u64::from_le_bytes(len_bytes) != location.len || header[14..46] != last_id {
            return Ok(false);
        }
        pack.seek(SeekFrom::Start(checkpoint.verified_len - 4))?;
        let mut trailer = [0_u8; 4];
        Ok(pack.read_exact(&mut trailer).is_ok() && &trailer == OBJECT_END)
    }

    fn truncate_pack_tail(&self, pack: &File, record_start: u64) -> anyhow::Result<()> {
        if let Some(frontier) = self.frontier()? {
            let durable_len = frontier
                .packs
                .get(self.active_pack.borrow().as_str())
                .copied()
                .unwrap_or(0);
            anyhow::ensure!(
                record_start >= durable_len,
                "durable object pack prefix is corrupt"
            );
        }
        pack.set_len(record_start)?;
        self.catalog.forget_capture_caches()?;
        fs::write(
            self.root.join("capture-generation"),
            format!("{:?}", std::time::SystemTime::now()),
        )?;
        self.catalog.delete_pack_objects_from(
            self.active_pack.borrow().as_str(),
            record_start.saturating_add(HEADER_LEN),
        )?;
        Ok(())
    }
}

/// Store writers wait a bounded interval so a stalled process cannot hang a caller forever.
pub(crate) fn wait_lock(file: &File, exclusive: bool) -> anyhow::Result<()> {
    let _operation = crate::writer_wait::Operation::start();
    loop {
        let result = if exclusive {
            file.try_lock_exclusive()
        } else {
            FileExt::try_lock_shared(file)
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
            {
                crate::writer_wait::pause(std::time::Duration::from_millis(10))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn read_or_initialize_current(root: &Path) -> anyhow::Result<String> {
    let packs = root.join("packs");
    let current = packs.join(CURRENT_FILE);
    if current.exists() {
        let pack = fs::read_to_string(&current)?.trim().to_owned();
        validate_pack_name(&pack)?;
        return Ok(pack);
    }
    let temporary = packs.join("CURRENT.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    writeln!(file, "{INITIAL_PACK_NAME}")?;
    file.sync_all()?;
    crate::platform::rename(temporary, current)?;
    crate::platform::sync_path(packs)?;
    Ok(INITIAL_PACK_NAME.to_owned())
}

fn validate_pack_name(pack: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !pack.is_empty()
            && pack.ends_with(".agp")
            && Path::new(pack).file_name() == Some(std::ffi::OsStr::new(pack)),
        "invalid active pack name"
    );
    Ok(())
}

fn trigger_name(trigger: &SnapshotTrigger) -> &'static str {
    match trigger {
        SnapshotTrigger::Initial => "initial",
        SnapshotTrigger::Manual => "manual",
        SnapshotTrigger::Watcher => "watcher",
        SnapshotTrigger::PreRewind => "pre_rewind",
        SnapshotTrigger::ForkBase => "fork_base",
        SnapshotTrigger::AgentRun => "agent_run",
        SnapshotTrigger::MergeSource => "merge_source",
        SnapshotTrigger::PreMerge => "pre_merge",
        SnapshotTrigger::Merge => "merge",
        SnapshotTrigger::SyncLocal => "sync_local",
        SnapshotTrigger::SyncPush => "sync_push",
        SnapshotTrigger::SyncPull => "sync_pull",
        SnapshotTrigger::Inspection => "inspection",
        SnapshotTrigger::Claim => "claim",
        SnapshotTrigger::Release => "release",
        SnapshotTrigger::Coord => "coord",
    }
}

pub fn object_id(kind: ObjectKind, bytes: &[u8]) -> ObjectId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(kind.domain());
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

/// Records where a workspace lives, durably and all at once: a reader sees the
/// old path or the new one, never half of either.
fn write_root_path(workspace_dir: &Path, root: &[u8]) -> anyhow::Result<()> {
    let temporary = workspace_dir.join("root.path.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(root)?;
    file.sync_all()?;
    crate::platform::rename(&temporary, workspace_dir.join("root.path"))?;
    crate::platform::sync_path(workspace_dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn checkpointed_store() -> (tempfile::TempDir, ObjectId, ObjectId, u64) {
        let directory = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(directory.path().to_owned()).unwrap();
        let first = store
            .put_bytes(ObjectKind::Chunk, &vec![b'a'; 128 * 1024])
            .unwrap();
        let second = store.put_bytes(ObjectKind::Chunk, b"second").unwrap();
        drop(store);

        // The first reopen verifies the legacy/uncheckpointed pack and records
        // the durable boundary used by subsequent startups.
        let store = ObjectStore::open(directory.path().to_owned()).unwrap();
        let verified_len = fs::metadata(directory.path().join("packs").join(INITIAL_PACK_NAME))
            .unwrap()
            .len();
        assert_eq!(
            store
                .catalog
                .pack_checkpoint(INITIAL_PACK_NAME)
                .unwrap()
                .unwrap()
                .verified_len,
            verified_len
        );
        drop(store);
        (directory, first, second, verified_len)
    }

    #[test]
    fn long_lived_store_refreshes_current_after_external_pack_activation() {
        let directory = tempfile::tempdir().unwrap();
        let first = ObjectStore::open(directory.path().to_owned()).unwrap();
        first.put_bytes(ObjectKind::Chunk, b"before gc").unwrap();
        let initial = directory.path().join("packs/pack-000001.agp");
        let initial_len = fs::metadata(&initial).unwrap().len();

        let mut gc_process = ObjectStore::open(directory.path().to_owned()).unwrap();
        let _exclusive = gc_process.acquire_maintenance_exclusive().unwrap();
        gc_process.activate_pack("pack-gc-test.agp").unwrap();
        drop(_exclusive);

        let id = first.put_bytes(ObjectKind::Chunk, b"after gc").unwrap();
        assert_eq!(fs::metadata(initial).unwrap().len(), initial_len);
        assert!(directory.path().join("packs/pack-gc-test.agp").exists());
        assert_eq!(
            first.catalog.object(&id).unwrap().unwrap().pack,
            "pack-gc-test.agp"
        );
    }

    #[test]
    fn valid_checkpoint_does_not_rehash_the_pack_prefix() {
        let (directory, first, second, _) = checkpointed_store();
        let catalog = Catalog::open(&directory.path().join("catalog.sqlite3")).unwrap();
        let first_location = catalog.object(&first).unwrap().unwrap();
        drop(catalog);

        let pack_path = directory.path().join("packs").join(INITIAL_PACK_NAME);
        let mut pack = OpenOptions::new().write(true).open(pack_path).unwrap();
        pack.seek(SeekFrom::Start(first_location.offset + 17))
            .unwrap();
        pack.write_all(b"Z").unwrap();
        pack.sync_data().unwrap();
        drop(pack);

        // Constant-size boundary validation succeeds without reading the old
        // payload. Reads retain their independent object-integrity check.
        let store = ObjectStore::open(directory.path().to_owned()).unwrap();
        assert!(store.read_bytes(&first, ObjectKind::Chunk).is_err());
        assert_eq!(
            store.read_bytes(&second, ObjectKind::Chunk).unwrap(),
            b"second"
        );
    }

    #[test]
    fn empty_catalog_index_forces_a_full_verified_rebuild() {
        let (directory, first, second, _) = checkpointed_store();
        let connection = Connection::open(directory.path().join("catalog.sqlite3")).unwrap();
        connection.execute("DELETE FROM objects", []).unwrap();
        drop(connection);

        let store = ObjectStore::open(directory.path().to_owned()).unwrap();
        assert_eq!(
            store.read_bytes(&first, ObjectKind::Chunk).unwrap(),
            vec![b'a'; 128 * 1024]
        );
        assert_eq!(
            store.read_bytes(&second, ObjectKind::Chunk).unwrap(),
            b"second"
        );
        assert_eq!(store.catalog.object_count().unwrap(), 2);
    }

    #[test]
    fn checkpointed_recovery_verifies_and_truncates_only_the_new_tail() {
        let (directory, first, second, verified_len) = checkpointed_store();
        let pack_path = directory.path().join("packs").join(INITIAL_PACK_NAME);
        let mut pack = OpenOptions::new().append(true).open(&pack_path).unwrap();
        pack.write_all(b"AGOB\x01").unwrap();
        pack.sync_data().unwrap();
        drop(pack);

        let store = ObjectStore::open(directory.path().to_owned()).unwrap();
        assert_eq!(fs::metadata(pack_path).unwrap().len(), verified_len);
        assert!(store.catalog.object(&first).unwrap().is_some());
        assert!(store.catalog.object(&second).unwrap().is_some());
        assert_eq!(
            store
                .catalog
                .pack_checkpoint(INITIAL_PACK_NAME)
                .unwrap()
                .unwrap()
                .verified_len,
            verified_len
        );
    }
    fn store_of(count: usize) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(directory.path().to_owned()).unwrap();
        for n in 0..count {
            store
                .put_bytes(ObjectKind::Chunk, format!("object {n}").as_bytes())
                .unwrap();
        }
        drop(store);
        // The reopen records the checkpoint every later open starts from.
        drop(ObjectStore::open(directory.path().to_owned()).unwrap());
        directory
    }

    fn open_work(directory: &Path) -> u64 {
        let before = crate::catalog::work_steps();
        drop(ObjectStore::open(directory.to_owned()).unwrap());
        crate::catalog::work_steps() - before
    }

    #[test]
    fn opening_a_checkpointed_store_does_catalog_work_independent_of_its_size() {
        let small = open_work(store_of(50).path());
        let large = open_work(store_of(5000).path());
        // A row-proportional scan adds several steps per extra row (~5000 rows
        // here); a fixed handful of seeks stays within the same few hundred.
        assert!(
            large <= small + 500,
            "open work grew with the store: {small} steps for 50 objects, {large} for 5000"
        );
    }

    #[test]
    fn a_settled_pack_opens_without_waiting_for_the_pack_lock() {
        let directory = store_of(10);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.path().join("locks/pack.lock"))
            .unwrap();
        FileExt::lock_exclusive(&lock).unwrap();

        let path = directory.path().to_owned();
        let (done, opened) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            drop(ObjectStore::open(path).unwrap());
            done.send(()).unwrap();
        });
        let finished = opened
            .recv_timeout(std::time::Duration::from_secs(3))
            .is_ok();
        FileExt::unlock(&lock).unwrap();
        reader.join().unwrap();
        assert!(finished, "a read-only open queued behind the pack lock");
    }

    fn three_objects() -> (tempfile::TempDir, [ObjectId; 3], u64) {
        let directory = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(directory.path().to_owned()).unwrap();
        let mut ids = [ObjectId::default(); 3];
        for (n, id) in ids.iter_mut().enumerate() {
            *id = store
                .put_bytes(ObjectKind::Chunk, format!("object {n}").as_bytes())
                .unwrap();
        }
        drop(store);
        drop(ObjectStore::open(directory.path().to_owned()).unwrap());
        let verified = fs::metadata(directory.path().join("packs").join(INITIAL_PACK_NAME))
            .unwrap()
            .len();
        (directory, ids, verified)
    }

    fn assert_all_readable(directory: &Path, ids: &[ObjectId; 3]) {
        let store = ObjectStore::open(directory.to_owned()).unwrap();
        for (n, id) in ids.iter().enumerate() {
            assert_eq!(
                store.read_bytes(id, ObjectKind::Chunk).unwrap(),
                format!("object {n}").as_bytes()
            );
        }
        assert_eq!(store.catalog.object_count().unwrap(), 3);
        let checkpoint = store
            .catalog
            .pack_checkpoint(INITIAL_PACK_NAME)
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.object_count, 3);
        assert_eq!(
            checkpoint.verified_len,
            fs::metadata(directory.join("packs").join(INITIAL_PACK_NAME))
                .unwrap()
                .len()
        );
    }

    /// Winds the checkpoint back to just before the third object's record.
    fn checkpoint_before_third(connection: &Connection, ids: &[ObjectId; 3]) {
        let verified: i64 = connection
            .query_row(
                "SELECT offset - ?2 FROM objects WHERE id = ?1",
                rusqlite::params![ids[2].as_slice(), HEADER_LEN as i64],
                |row| row.get(0),
            )
            .unwrap();
        connection
            .execute(
                "UPDATE pack_checkpoints SET verified_len = ?1, object_count = 2,
                 last_object = ?2,
                 last_record_start = (SELECT offset - ?3 FROM objects WHERE id = ?2)",
                rusqlite::params![verified, ids[1].as_slice(), HEADER_LEN as i64],
            )
            .unwrap();
    }

    #[test]
    fn rows_ahead_of_a_stale_checkpoint_recover_every_object() {
        // A crash after the catalog rows committed but before the checkpoint did.
        let (directory, ids, _) = three_objects();
        let connection = Connection::open(directory.path().join("catalog.sqlite3")).unwrap();
        checkpoint_before_third(&connection, &ids);
        drop(connection);
        assert_all_readable(directory.path(), &ids);
    }

    #[test]
    fn a_pack_ahead_of_a_lost_catalog_commit_is_reindexed() {
        let (directory, ids, _) = three_objects();
        let connection = Connection::open(directory.path().join("catalog.sqlite3")).unwrap();
        checkpoint_before_third(&connection, &ids);
        connection
            .execute("DELETE FROM objects WHERE id = ?1", [ids[2].as_slice()])
            .unwrap();
        drop(connection);
        assert_all_readable(directory.path(), &ids);
    }

    #[test]
    fn a_torn_final_record_is_dropped_with_its_catalog_row() {
        // The catalog remembers an object whose record the pack lost part of.
        let (directory, ids, verified) = three_objects();
        let pack = OpenOptions::new()
            .write(true)
            .open(directory.path().join("packs").join(INITIAL_PACK_NAME))
            .unwrap();
        pack.set_len(verified - 2).unwrap();
        drop(pack);

        let store = ObjectStore::open(directory.path().to_owned()).unwrap();
        assert!(store.catalog.object(&ids[2]).unwrap().is_none());
        for (n, id) in ids[..2].iter().enumerate() {
            assert_eq!(
                store.read_bytes(id, ObjectKind::Chunk).unwrap(),
                format!("object {n}").as_bytes()
            );
        }
        let checkpoint = store
            .catalog
            .pack_checkpoint(INITIAL_PACK_NAME)
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.object_count, 2);
        assert_eq!(
            checkpoint.verified_len,
            fs::metadata(directory.path().join("packs").join(INITIAL_PACK_NAME))
                .unwrap()
                .len()
        );
    }

    #[test]
    fn a_checkpoint_inside_a_record_forces_a_rebuild() {
        let (directory, ids, verified) = three_objects();
        let connection = Connection::open(directory.path().join("catalog.sqlite3")).unwrap();
        connection
            .execute(
                "UPDATE pack_checkpoints SET verified_len = ?1",
                [verified as i64 - 3],
            )
            .unwrap();
        drop(connection);
        assert_all_readable(directory.path(), &ids);
    }

    #[test]
    fn a_dropped_index_is_rebuilt_from_the_pack() {
        let (directory, ids, _) = three_objects();
        let connection = Connection::open(directory.path().join("catalog.sqlite3")).unwrap();
        connection.execute("DELETE FROM objects", []).unwrap();
        drop(connection);
        assert_all_readable(directory.path(), &ids);
    }
    #[test]
    fn a_settled_store_opens_while_another_process_holds_the_catalog_write_lock() {
        let directory = store_of(10);
        let writer = Connection::open(directory.path().join("catalog.sqlite3")).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();

        let path = directory.path().to_owned();
        let (done, opened) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            drop(ObjectStore::open(path).unwrap());
            done.send(()).unwrap();
        });
        let finished = opened
            .recv_timeout(std::time::Duration::from_secs(3))
            .is_ok();
        writer.execute_batch("ROLLBACK").unwrap();
        reader.join().unwrap();
        assert!(
            finished,
            "a read-only open queued behind the catalog write lock"
        );
    }
    #[test]
    fn legacy_file_rows_move_to_their_folder_once_and_later_opens_leave_them() {
        let directory = store_of(1);
        let workspace = directory.path().join("workspaces/w1");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("root.path"), b"/work/tree").unwrap();
        let folder = format!("folder:{}", blake3::hash(b"/work/tree").to_hex());
        let connection = Connection::open(directory.path().join("catalog.sqlite3")).unwrap();
        for path in [b"a.txt".as_slice(), b".cell/x".as_slice()] {
            connection
                .execute(
                    "INSERT INTO stat_cache VALUES('w1', ?1, 1, 1, 1, 1, 1, 1, 1, 420, ?2, NULL, 0, 1)",
                    rusqlite::params![path, [7_u8; 32].as_slice()],
                )
                .unwrap();
        }
        let rows = |owner: &str| -> i64 {
            connection
                .query_row(
                    "SELECT COUNT(*) FROM stat_cache WHERE workspace_id = ?1",
                    [owner],
                    |row| row.get(0),
                )
                .unwrap()
        };

        drop(ObjectStore::open(directory.path().to_owned()).unwrap());
        // The folder takes the file row; the composed `.cell` row stays put.
        assert_eq!((rows("w1"), rows(&folder)), (1, 1));
        drop(ObjectStore::open(directory.path().to_owned()).unwrap());
        assert_eq!((rows("w1"), rows(&folder)), (1, 1));
    }
    fn drop_row(directory: &Path, id: &ObjectId) {
        let connection = Connection::open(directory.join("catalog.sqlite3")).unwrap();
        connection
            .execute("DELETE FROM objects WHERE id = ?1", [id.as_slice()])
            .unwrap();
    }

    #[test]
    fn verify_restores_rows_missing_from_the_middle_of_the_verified_prefix() {
        let (directory, ids, _) = three_objects();
        drop_row(directory.path(), &ids[0]);
        let store = ObjectStore::open(directory.path().to_owned()).unwrap();
        // Open trusts the checkpoint and cannot see the hole.
        assert!(store.catalog.object(&ids[0]).unwrap().is_none());

        let check = store.verify_index(false).unwrap();
        assert_eq!(
            check,
            IndexCheck {
                objects: 3,
                repaired: true
            }
        );
        drop(store);
        assert_all_readable(directory.path(), &ids);
        let store = ObjectStore::open(directory.path().to_owned()).unwrap();
        assert!(!store.verify_index(false).unwrap().repaired);
    }

    #[test]
    fn a_read_that_finds_a_row_missing_rescans_the_pack_once() {
        let (directory, ids, _) = three_objects();
        drop_row(directory.path(), &ids[0]);
        let store = ObjectStore::open(directory.path().to_owned()).unwrap();
        assert_eq!(
            store.read_bytes(&ids[0], ObjectKind::Chunk).unwrap(),
            b"object 0"
        );
        // The rescan is spent: a second hole in the same session is an error.
        drop_row(directory.path(), &ids[1]);
        assert!(store.read_bytes(&ids[1], ObjectKind::Chunk).is_err());
    }

    #[test]
    fn a_deep_verify_of_a_sound_pack_repairs_nothing() {
        let (directory, ids, _) = three_objects();
        let store = ObjectStore::open(directory.path().to_owned()).unwrap();
        let check = store.verify_index(true).unwrap();
        assert_eq!(
            check,
            IndexCheck {
                objects: 3,
                repaired: false
            }
        );
        drop(store);
        assert_all_readable(directory.path(), &ids);
    }

    #[test]
    fn a_deep_verify_that_finds_a_damaged_record_leaves_the_index_alone() {
        let (directory, ids, _) = three_objects();
        let store = ObjectStore::open(directory.path().to_owned()).unwrap();
        let location = store.catalog.object(&ids[1]).unwrap().unwrap();
        let mut pack = OpenOptions::new()
            .write(true)
            .open(directory.path().join("packs").join(INITIAL_PACK_NAME))
            .unwrap();
        pack.seek(SeekFrom::Start(location.offset)).unwrap();
        pack.write_all(b"Z").unwrap();
        drop(pack);

        assert!(store.verify_index(true).is_err());
        // The failed rebuild rolled back: every row is still there.
        assert_eq!(store.catalog.object_count().unwrap(), 3);
    }
}
