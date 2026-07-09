use crate::catalog::{CachedFile, Catalog, TimelineRow};
use crate::model::{ObjectId, ObjectKind, SnapshotTrigger};
use crate::refs::{RefLog, RefRecord};
use anyhow::Context;
use fs2::FileExt;
use serde::{de::DeserializeOwned, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const PACK_NAME: &str = "pack-000001.agp";
const OBJECT_MAGIC: &[u8; 4] = b"AGOB";
const OBJECT_END: &[u8; 4] = b"AGND";
const OBJECT_VERSION: u8 = 1;
const HEADER_LEN: u64 = 4 + 1 + 1 + 8 + 32 + 32;
const MAX_OBJECT_LEN: u64 = 256 * 1024 * 1024;

pub struct ObjectStore {
    root: PathBuf,
    catalog: Catalog,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct StoreStats {
    pub objects: u64,
    pub physical_bytes: u64,
}

impl ObjectStore {
    pub fn open(root: PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(root.join("packs"))?;
        fs::create_dir_all(root.join("locks"))?;
        fs::create_dir_all(root.join("workspaces"))?;
        let catalog = Catalog::open(&root.join("catalog.sqlite3"))?;
        let store = Self { root, catalog };
        store.recover_pack()?;
        Ok(store)
    }

    pub fn put_bytes(&self, kind: ObjectKind, bytes: &[u8]) -> anyhow::Result<ObjectId> {
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_OBJECT_LEN,
            "object exceeds size limit"
        );
        let id = object_id(kind, bytes);
        if self.catalog.object(&id)?.is_some() {
            return Ok(id);
        }

        let lock_path = self.root.join("locks").join("pack.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        lock.lock_exclusive()?;

        if self.catalog.object(&id)?.is_none() {
            let pack_path = self.root.join("packs").join(PACK_NAME);
            let mut pack = OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&pack_path)?;
            let record_start = pack.seek(SeekFrom::End(0))?;
            let checksum = blake3::hash(bytes);
            pack.write_all(OBJECT_MAGIC)?;
            pack.write_all(&[OBJECT_VERSION, kind as u8])?;
            pack.write_all(&(bytes.len() as u64).to_le_bytes())?;
            pack.write_all(&id)?;
            pack.write_all(checksum.as_bytes())?;
            let payload_offset = record_start + HEADER_LEN;
            pack.write_all(bytes)?;
            pack.write_all(OBJECT_END)?;
            self.catalog
                .insert_object(&id, kind, PACK_NAME, payload_offset, bytes.len() as u64)?;
        }
        FileExt::unlock(&lock)?;
        Ok(id)
    }

    pub fn put_struct<T: Serialize>(
        &self,
        kind: ObjectKind,
        value: &T,
    ) -> anyhow::Result<ObjectId> {
        let bytes = serde_json::to_vec(value)?;
        self.put_bytes(kind, &bytes)
    }

    pub fn read_bytes(&self, id: &ObjectId, expected: ObjectKind) -> anyhow::Result<Vec<u8>> {
        let location = self
            .catalog
            .object(id)?
            .with_context(|| format!("missing object {}", hex::encode(id)))?;
        anyhow::ensure!(location.kind == expected, "object kind mismatch");
        let mut pack = File::open(self.root.join("packs").join(location.pack))?;
        pack.seek(SeekFrom::Start(location.offset))?;
        let mut bytes = vec![0_u8; location.len as usize];
        pack.read_exact(&mut bytes)?;
        anyhow::ensure!(
            object_id(expected, &bytes) == *id,
            "object integrity check failed"
        );
        Ok(bytes)
    }

    pub fn read_struct<T: DeserializeOwned>(
        &self,
        id: &ObjectId,
        expected: ObjectKind,
    ) -> anyhow::Result<T> {
        Ok(serde_json::from_slice(&self.read_bytes(id, expected)?)?)
    }

    pub fn ensure_workspace(&mut self, id: &str, root: &[u8]) -> anyhow::Result<()> {
        let workspace_dir = self.root.join("workspaces").join(id);
        fs::create_dir_all(&workspace_dir)?;
        let metadata_path = workspace_dir.join("root.path");
        if !metadata_path.exists() {
            let temporary = workspace_dir.join("root.path.tmp");
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(root)?;
            file.sync_all()?;
            fs::rename(&temporary, &metadata_path)?;
            File::open(&workspace_dir)?.sync_all()?;
        }
        self.catalog.ensure_workspace(id, root)?;
        for record in RefLog::open(&self.root, id)?.records()? {
            self.index_ref_record(id, &record)?;
        }
        Ok(())
    }

    pub fn find_workspace(&self, root: &[u8]) -> anyhow::Result<Option<String>> {
        for entry in fs::read_dir(self.root.join("workspaces"))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
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
        Ok(RefLog::open(&self.root, id)?
            .records()?
            .last()
            .map(|record| record.snapshot_id))
    }

    pub fn timeline(&self, id: &str, limit: usize) -> anyhow::Result<Vec<TimelineRow>> {
        let records = RefLog::open(&self.root, id)?.records()?;
        Ok(records
            .into_iter()
            .rev()
            .take(limit)
            .map(|record| TimelineRow {
                id: record.snapshot_id,
                sealed_at: record.sealed_at,
                label: record.label,
                trigger: trigger_name(&record.trigger).to_owned(),
            })
            .collect())
    }

    pub fn cached_file(
        &self,
        workspace_id: &str,
        path: &[u8],
    ) -> anyhow::Result<Option<CachedFile>> {
        self.catalog.cached_file(workspace_id, path)
    }

    pub fn cache_file(
        &self,
        workspace_id: &str,
        path: &[u8],
        file: &CachedFile,
    ) -> anyhow::Result<()> {
        self.catalog.cache_file(workspace_id, path, file)
    }

    pub fn publish_snapshot(
        &mut self,
        workspace_id: &str,
        snapshot_id: ObjectId,
        sealed_at: i64,
        label: Option<String>,
        trigger: SnapshotTrigger,
    ) -> anyhow::Result<()> {
        // The append is the visibility boundary. Every referenced object was
        // sync'd before this call; SQLite is an advisory index rebuilt from it.
        self.sync_active_pack()?;
        let record = RefLog::open(&self.root, workspace_id)?.append(
            snapshot_id,
            sealed_at,
            label,
            trigger,
        )?;
        self.index_ref_record(workspace_id, &record)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace_data_dir(&self, workspace_id: &str) -> PathBuf {
        self.root.join("workspaces").join(workspace_id)
    }

    pub fn stats(&self) -> anyhow::Result<StoreStats> {
        let mut physical_bytes = 0_u64;
        for entry in fs::read_dir(self.root.join("packs"))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                physical_bytes += entry.metadata()?.len();
            }
        }
        Ok(StoreStats {
            objects: self.catalog.object_count()?,
            physical_bytes,
        })
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
        let path = self.root.join("packs").join(PACK_NAME);
        if path.exists() {
            File::open(path)?.sync_data()?;
        }
        Ok(())
    }

    fn recover_pack(&self) -> anyhow::Result<()> {
        let pack_path = self.root.join("packs").join(PACK_NAME);
        if !pack_path.exists() {
            return Ok(());
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("locks").join("pack.lock"))?;
        lock.lock_exclusive()?;
        let mut pack = OpenOptions::new().read(true).write(true).open(&pack_path)?;
        let file_len = pack.metadata()?.len();
        let mut offset = 0_u64;
        while offset < file_len {
            let record_start = offset;
            let mut magic = [0_u8; 4];
            if pack.read_exact(&mut magic).is_err() {
                self.truncate_pack_tail(&pack, record_start)?;
                break;
            }
            anyhow::ensure!(
                &magic == OBJECT_MAGIC,
                "object pack corruption at byte {record_start}"
            );
            let mut meta = [0_u8; 2];
            if pack.read_exact(&mut meta).is_err() {
                self.truncate_pack_tail(&pack, record_start)?;
                break;
            }
            anyhow::ensure!(meta[0] == OBJECT_VERSION, "unsupported object pack version");
            let kind = ObjectKind::from_u8(meta[1]).context("invalid object kind in pack")?;
            let mut len_bytes = [0_u8; 8];
            if pack.read_exact(&mut len_bytes).is_err() {
                self.truncate_pack_tail(&pack, record_start)?;
                break;
            }
            let len = u64::from_le_bytes(len_bytes);
            anyhow::ensure!(
                len <= MAX_OBJECT_LEN,
                "object pack record exceeds size limit"
            );
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
            let mut buffer = vec![0_u8; 1024 * 1024];
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
            anyhow::ensure!(&end == OBJECT_END, "object pack trailer mismatch");
            anyhow::ensure!(
                checksum_hasher.finalize().as_bytes() == &checksum,
                "object payload checksum mismatch"
            );
            anyhow::ensure!(
                content_hasher.finalize().as_bytes() == &id,
                "object ID mismatch"
            );
            self.catalog
                .insert_object(&id, kind, PACK_NAME, payload_offset, len)?;
            offset = pack.stream_position()?;
        }
        pack.sync_data()?;
        FileExt::unlock(&lock)?;
        Ok(())
    }

    fn truncate_pack_tail(&self, pack: &File, record_start: u64) -> anyhow::Result<()> {
        pack.set_len(record_start)?;
        self.catalog
            .delete_pack_objects_from(PACK_NAME, record_start.saturating_add(HEADER_LEN))?;
        Ok(())
    }
}

fn trigger_name(trigger: &SnapshotTrigger) -> &'static str {
    match trigger {
        SnapshotTrigger::Initial => "initial",
        SnapshotTrigger::Manual => "manual",
        SnapshotTrigger::Watcher => "watcher",
        SnapshotTrigger::PreRewind => "pre_rewind",
    }
}

pub fn object_id(kind: ObjectKind, bytes: &[u8]) -> ObjectId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(kind.domain());
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}
