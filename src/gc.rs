//! Exact, bounded-memory reachability collection for the global object store.

use crate::catalog::PackCheckpoint;
use crate::model::{Blob, EntryKind, ObjectId, ObjectKind, Snapshot, Tree};
use crate::refs::RefLog;
use crate::store::ObjectStore;
use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const OBJECT_MAGIC: &[u8; 4] = b"AGOB";
const OBJECT_END: &[u8; 4] = b"AGND";
const OBJECT_VERSION: u8 = 1;
const HEADER_LEN: u64 = 4 + 1 + 1 + 8 + 32 + 32;
const RECORD_OVERHEAD: u64 = HEADER_LEN + OBJECT_END.len() as u64;
const COMPACTION_BATCH: usize = 1_024;

#[derive(Debug, Clone, Serialize)]
pub struct GcReport {
    pub dry_run: bool,
    pub roots: u64,
    pub reachable_objects: u64,
    pub unreachable_objects: u64,
    pub reachable_payload_bytes: u64,
    pub unreachable_payload_bytes: u64,
    pub physical_bytes_before: u64,
    pub physical_bytes_after: u64,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Deserialize)]
struct RestoreRoots {
    pre_snapshot: ObjectId,
    target_snapshot: ObjectId,
}

#[derive(Debug, Deserialize)]
struct SyncRoot {
    snapshot: ObjectId,
}

struct MarkDatabase {
    path: PathBuf,
    connection: Connection,
}

impl MarkDatabase {
    fn create(store_root: &Path) -> anyhow::Result<Self> {
        let path = store_root.join("locks").join(format!(
            "gc-mark-{}-{}.sqlite3",
            std::process::id(),
            unique_generation()
        ));
        let connection = Connection::open(&path)?;
        connection.pragma_update(None, "journal_mode", "OFF")?;
        connection.pragma_update(None, "synchronous", "OFF")?;
        connection.execute_batch(
            "CREATE TABLE marks (
                id BLOB PRIMARY KEY,
                kind INTEGER NOT NULL,
                processed INTEGER NOT NULL DEFAULT 0
             ) WITHOUT ROWID;
             CREATE INDEX marks_pending ON marks(processed, id);
             CREATE TABLE locations (
                id BLOB PRIMARY KEY,
                kind INTEGER NOT NULL,
                offset INTEGER NOT NULL,
                len INTEGER NOT NULL
             ) WITHOUT ROWID;",
        )?;
        Ok(Self { path, connection })
    }

    fn enqueue(&self, id: &ObjectId, kind: ObjectKind) -> anyhow::Result<bool> {
        Ok(self.connection.execute(
            "INSERT OR IGNORE INTO marks(id, kind) VALUES(?1, ?2)",
            params![id.as_slice(), kind as u8],
        )? != 0)
    }

    fn next(&self) -> anyhow::Result<Option<(ObjectId, ObjectKind)>> {
        self.connection
            .query_row(
                "SELECT id, kind FROM marks WHERE processed = 0 ORDER BY id LIMIT 1",
                [],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, u8>(1)?)),
            )
            .optional()?
            .map(|(bytes, kind)| {
                let id = vec_to_id(bytes)?;
                let kind = ObjectKind::from_u8(kind).context("invalid kind in GC mark database")?;
                Ok((id, kind))
            })
            .transpose()
    }

    fn processed(&self, id: &ObjectId) -> anyhow::Result<()> {
        self.connection.execute(
            "UPDATE marks SET processed = 1 WHERE id = ?1",
            params![id.as_slice()],
        )?;
        Ok(())
    }

    fn counts(&self) -> anyhow::Result<(u64, u64)> {
        let count = self
            .connection
            .query_row("SELECT COUNT(*) FROM marks", [], |row| row.get::<_, i64>(0))?;
        Ok((count as u64, 0))
    }
}

impl Drop for MarkDatabase {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(self.path.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(self.path.with_extension("sqlite3-shm"));
    }
}

pub fn collect(store: &mut ObjectStore, dry_run: bool) -> anyhow::Result<GcReport> {
    let _maintenance = store.acquire_maintenance_exclusive()?;
    collect_locked(store, dry_run)
}

fn collect_locked(store: &mut ObjectStore, dry_run: bool) -> anyhow::Result<GcReport> {
    let marks = MarkDatabase::create(store.root())?;
    let roots = enqueue_roots(store.root(), &marks)?;
    traverse(store, &marks)?;

    let reachable_objects = marks.counts()?.0;
    let total_objects = store.stats_unlocked()?.objects;
    let total_payload = store.object_payload_bytes()?;
    let reachable_payload = reachable_payload_bytes(store, &marks)?;
    let before = store.stats_unlocked()?.physical_bytes;
    let projected = reachable_payload.saturating_add(reachable_objects * RECORD_OVERHEAD);
    let unreachable_objects = total_objects.saturating_sub(reachable_objects);
    let unreachable_payload = total_payload.saturating_sub(reachable_payload);

    if dry_run {
        return Ok(GcReport {
            dry_run: true,
            roots,
            reachable_objects,
            unreachable_objects,
            reachable_payload_bytes: reachable_payload,
            unreachable_payload_bytes: unreachable_payload,
            physical_bytes_before: before,
            physical_bytes_after: projected,
            reclaimed_bytes: before.saturating_sub(projected),
        });
    }

    let after = compact(store, &marks)?;
    Ok(GcReport {
        dry_run: false,
        roots,
        reachable_objects,
        unreachable_objects,
        reachable_payload_bytes: reachable_payload,
        unreachable_payload_bytes: unreachable_payload,
        physical_bytes_before: before,
        physical_bytes_after: after,
        reclaimed_bytes: before.saturating_sub(after),
    })
}

fn enqueue_roots(store_root: &Path, marks: &MarkDatabase) -> anyhow::Result<u64> {
    let mut roots = 0_u64;
    let workspaces = store_root.join("workspaces");
    if !workspaces.exists() {
        return Ok(0);
    }
    for entry in fs::read_dir(workspaces)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let workspace_id = entry.file_name().to_string_lossy().into_owned();
        let refs = RefLog::open(store_root, &workspace_id)?;
        refs.for_each_snapshot(|id| {
            if marks.enqueue(&id, ObjectKind::Snapshot)? {
                roots += 1;
            }
            Ok(())
        })?;

        let intent_path = entry.path().join("restore.intent");
        if intent_path.exists() {
            let intent: RestoreRoots = serde_json::from_slice(&fs::read(&intent_path)?)
                .with_context(|| format!("read restore intent {}", intent_path.display()))?;
            for id in [intent.pre_snapshot, intent.target_snapshot] {
                if marks.enqueue(&id, ObjectKind::Snapshot)? {
                    roots += 1;
                }
            }
        }

        let incoming_path = entry.path().join("sync/incoming.json");
        if incoming_path.exists() {
            let incoming: SyncRoot = serde_json::from_slice(&fs::read(&incoming_path)?)
                .with_context(|| format!("read incoming sync root {}", incoming_path.display()))?;
            if marks.enqueue(&incoming.snapshot, ObjectKind::Snapshot)? {
                roots += 1;
            }
        }
    }
    Ok(roots)
}

fn traverse(store: &ObjectStore, marks: &MarkDatabase) -> anyhow::Result<()> {
    while let Some((id, expected)) = marks.next()? {
        let actual = store.object_kind(&id)?;
        anyhow::ensure!(actual == expected, "reachable object kind mismatch");
        let bytes = store.read_bytes_unlocked(&id, expected)?;
        match expected {
            ObjectKind::Snapshot => {
                let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
                marks.enqueue(&snapshot.root_tree, ObjectKind::Tree)?;
                for backup in snapshot.sqlite_backups {
                    marks.enqueue(&backup.blob, ObjectKind::Blob)?;
                }
                // Snapshot.parent is timeline metadata, not a reachability edge.
            }
            ObjectKind::Tree => {
                let tree: Tree = serde_json::from_slice(&bytes)?;
                for page in tree.pages {
                    marks.enqueue(&page.target, ObjectKind::Tree)?;
                }
                for entry in tree.entries {
                    if let Some(xattrs) = entry.xattrs {
                        marks.enqueue(&xattrs, ObjectKind::Xattrs)?;
                    }
                    match (entry.kind, entry.target) {
                        (EntryKind::File, Some(target)) => {
                            marks.enqueue(&target, ObjectKind::Blob)?;
                        }
                        (EntryKind::Directory, Some(target)) => {
                            marks.enqueue(&target, ObjectKind::Tree)?;
                        }
                        _ => {}
                    }
                }
            }
            ObjectKind::Blob => {
                let blob: Blob = serde_json::from_slice(&bytes)?;
                for chunk in blob.chunks {
                    marks.enqueue(&chunk.id, ObjectKind::Chunk)?;
                }
            }
            ObjectKind::Chunk | ObjectKind::Xattrs => {}
        }
        marks.processed(&id)?;
    }
    Ok(())
}

fn reachable_payload_bytes(store: &ObjectStore, marks: &MarkDatabase) -> anyhow::Result<u64> {
    let mut total = 0_u64;
    let mut last: Option<Vec<u8>> = None;
    loop {
        let batch = mark_batch(&marks.connection, last.as_deref())?;
        if batch.is_empty() {
            break;
        }
        for (id, _) in &batch {
            total = total.saturating_add(store.object_len(id)?);
        }
        last = batch.last().map(|(id, _)| id.to_vec());
    }
    Ok(total)
}

fn compact(store: &mut ObjectStore, marks: &MarkDatabase) -> anyhow::Result<u64> {
    let generation = unique_generation();
    let pack_name = format!("pack-gc-{generation}.agp");
    let packs = store.root().join("packs");
    let temporary = packs.join(format!(".{pack_name}.tmp"));
    let final_path = packs.join(&pack_name);
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let mut count = 0_u64;
    let mut position = 0_u64;
    let mut last_id = None;
    let mut last_record_start = 0_u64;

    loop {
        let batch = mark_batch(&marks.connection, last_id.as_deref())?;
        if batch.is_empty() {
            break;
        }
        for (id, expected) in &batch {
            let bytes = store.read_bytes_unlocked(id, *expected)?;
            let record_start = position;
            let payload_offset = write_record(&mut output, *expected, id, &bytes)?;
            position = payload_offset + bytes.len() as u64 + OBJECT_END.len() as u64;
            marks.connection.execute(
                "INSERT INTO locations(id, kind, offset, len) VALUES(?1, ?2, ?3, ?4)",
                params![
                    id.as_slice(),
                    *expected as u8,
                    payload_offset as i64,
                    bytes.len() as i64
                ],
            )?;
            count += 1;
            last_record_start = record_start;
        }
        last_id = batch.last().map(|(id, _)| id.to_vec());
    }
    output.sync_all()?;
    drop(output);
    fs::rename(&temporary, &final_path)?;
    File::open(&packs)?.sync_all()?;

    if std::env::var_os("AGIT_GC_FAILPOINT").as_deref()
        == Some(std::ffi::OsStr::new("after_pack_publish"))
    {
        anyhow::bail!("injected GC failure after pack publish");
    }

    let checkpoint = PackCheckpoint {
        verified_len: position,
        object_count: count,
        last_object: last_id.map(vec_to_id).transpose()?,
        last_record_start,
    };
    store.replace_objects_from_gc(&marks.path, &pack_name, &checkpoint)?;

    if std::env::var_os("AGIT_GC_FAILPOINT").as_deref()
        == Some(std::ffi::OsStr::new("after_catalog_swap"))
    {
        anyhow::bail!("injected GC failure after catalog swap");
    }

    // CURRENT is the recovery boundary. Before this rename the old generation
    // can rebuild a deleted catalog; afterward the new compacted generation can.
    store.activate_pack(&pack_name)?;

    for entry in fs::read_dir(&packs)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry.path().extension() == Some(std::ffi::OsStr::new("agp"))
            && entry.file_name() != std::ffi::OsStr::new(&pack_name)
        {
            fs::remove_file(entry.path())?;
        }
    }
    File::open(&packs)?.sync_all()?;
    Ok(fs::metadata(final_path)?.len())
}

fn mark_batch(
    connection: &Connection,
    after: Option<&[u8]>,
) -> anyhow::Result<Vec<(ObjectId, ObjectKind)>> {
    let mut statement = if after.is_some() {
        connection.prepare("SELECT id, kind FROM marks WHERE id > ?1 ORDER BY id LIMIT ?2")?
    } else {
        connection.prepare("SELECT id, kind FROM marks ORDER BY id LIMIT ?1")?
    };
    let collect = |row: &rusqlite::Row<'_>| -> rusqlite::Result<(Vec<u8>, u8)> {
        Ok((row.get(0)?, row.get(1)?))
    };
    let mut raw = Vec::new();
    if let Some(after) = after {
        for row in statement.query_map(params![after, COMPACTION_BATCH as i64], collect)? {
            raw.push(row?);
        }
    } else {
        for row in statement.query_map(params![COMPACTION_BATCH as i64], collect)? {
            raw.push(row?);
        }
    }
    raw.into_iter()
        .map(|(id, kind)| {
            Ok((
                vec_to_id(id)?,
                ObjectKind::from_u8(kind).context("invalid marked object kind")?,
            ))
        })
        .collect()
}

fn write_record(
    output: &mut File,
    kind: ObjectKind,
    id: &ObjectId,
    bytes: &[u8],
) -> anyhow::Result<u64> {
    let checksum = blake3::hash(bytes);
    output.write_all(OBJECT_MAGIC)?;
    output.write_all(&[OBJECT_VERSION, kind as u8])?;
    output.write_all(&(bytes.len() as u64).to_le_bytes())?;
    output.write_all(id)?;
    output.write_all(checksum.as_bytes())?;
    output.write_all(bytes)?;
    output.write_all(OBJECT_END)?;
    Ok(output.stream_position()? - bytes.len() as u64 - OBJECT_END.len() as u64)
}

fn unique_generation() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn vec_to_id(bytes: Vec<u8>) -> anyhow::Result<ObjectId> {
    anyhow::ensure!(bytes.len() == 32, "invalid object ID in GC database");
    let mut id = [0_u8; 32];
    id.copy_from_slice(&bytes);
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        ChunkRef, SealQuality, SnapshotTrigger, SqliteBackup, TreeEntry, TreePage, XattrEntry,
        Xattrs,
    };

    fn empty_tree(store: &ObjectStore) -> ObjectId {
        store
            .put_struct(
                ObjectKind::Tree,
                &Tree {
                    entries: Vec::new(),
                    pages: Vec::new(),
                },
            )
            .unwrap()
    }

    fn snapshot(root_tree: ObjectId, parent: Option<ObjectId>) -> Snapshot {
        Snapshot {
            root_tree,
            parent,
            merge_parents: Vec::new(),
            sealed_at_secs: 1,
            sealed_at_nanos: 0,
            quality: SealQuality::Quiescent,
            trigger: SnapshotTrigger::Manual,
            label: None,
            sqlite_backups: Vec::new(),
            claims: Vec::new(),
        }
    }

    #[test]
    fn exact_gc_follows_pages_and_chunks_but_not_snapshot_parent() {
        let temporary = tempfile::tempdir().unwrap();
        let mut store = ObjectStore::open(temporary.path().to_owned()).unwrap();
        store.ensure_workspace("workspace", b"/workspace").unwrap();

        let kept_chunk = store.put_bytes(ObjectKind::Chunk, b"kept bytes").unwrap();
        let kept_blob = store
            .put_struct(
                ObjectKind::Blob,
                &Blob {
                    chunks: vec![ChunkRef {
                        id: kept_chunk,
                        len: 10,
                    }],
                    total_len: 10,
                },
            )
            .unwrap();
        let xattrs = store
            .put_struct(
                ObjectKind::Xattrs,
                &Xattrs {
                    entries: vec![XattrEntry {
                        name: b"user.test".to_vec(),
                        value: b"yes".to_vec(),
                    }],
                },
            )
            .unwrap();
        let leaf = store
            .put_struct(
                ObjectKind::Tree,
                &Tree {
                    entries: vec![TreeEntry {
                        name: b"file".to_vec(),
                        kind: EntryKind::File,
                        target: Some(kept_blob),
                        link_target: Vec::new(),
                        mode: 0o100644,
                        size: 10,
                        mtime_secs: 1,
                        mtime_nanos: 0,
                        xattrs: Some(xattrs),
                    }],
                    pages: Vec::new(),
                },
            )
            .unwrap();
        let root = store
            .put_struct(
                ObjectKind::Tree,
                &Tree {
                    entries: Vec::new(),
                    pages: vec![TreePage {
                        first_name: b"file".to_vec(),
                        last_name: b"file".to_vec(),
                        entry_count: 1,
                        target: leaf,
                    }],
                },
            )
            .unwrap();

        let orphan_tree = empty_tree(&store);
        let orphan_parent = store
            .put_struct(ObjectKind::Snapshot, &snapshot(orphan_tree, None))
            .unwrap();
        let abandoned = store
            .put_bytes(ObjectKind::Chunk, b"not referenced")
            .unwrap();
        let mut current = snapshot(root, Some(orphan_parent));
        current.sqlite_backups.push(SqliteBackup {
            path: b"dev.sqlite".to_vec(),
            blob: kept_blob,
            integrity_ok: true,
        });
        let current = store.put_struct(ObjectKind::Snapshot, &current).unwrap();
        store
            .publish_snapshot("workspace", current, 1, None, SnapshotTrigger::Manual)
            .unwrap();

        let bytes_before_preview = store.stats().unwrap().physical_bytes;
        let preview = collect(&mut store, true).unwrap();
        assert!(preview.dry_run);
        assert_eq!(preview.reachable_objects, 6);
        assert_eq!(preview.unreachable_objects, 3);
        assert!(store.read_bytes(&abandoned, ObjectKind::Chunk).is_ok());
        assert_eq!(store.stats().unwrap().physical_bytes, bytes_before_preview);

        let report = collect(&mut store, false).unwrap();
        assert_eq!(report.reachable_objects, 6);
        assert_eq!(report.unreachable_objects, 3);
        assert!(store.read_bytes(&kept_chunk, ObjectKind::Chunk).is_ok());
        assert!(store.read_bytes(&leaf, ObjectKind::Tree).is_ok());
        assert!(store
            .read_bytes(&orphan_parent, ObjectKind::Snapshot)
            .is_err());
        assert!(store.read_bytes(&orphan_tree, ObjectKind::Tree).is_err());
        assert!(store.read_bytes(&abandoned, ObjectKind::Chunk).is_err());
        assert!(report.physical_bytes_after < report.physical_bytes_before);

        let appended = store
            .put_bytes(ObjectKind::Chunk, b"written after compaction")
            .unwrap();
        drop(store);
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(temporary.path().join(format!("catalog.sqlite3{suffix}")));
        }
        let rebuilt = ObjectStore::open(temporary.path().to_owned()).unwrap();
        assert!(rebuilt.read_bytes(&kept_chunk, ObjectKind::Chunk).is_ok());
        assert!(rebuilt.read_bytes(&leaf, ObjectKind::Tree).is_ok());
        assert_eq!(
            rebuilt.read_bytes(&appended, ObjectKind::Chunk).unwrap(),
            b"written after compaction"
        );
    }

    #[test]
    fn restore_intent_is_an_independent_gc_root() {
        let temporary = tempfile::tempdir().unwrap();
        let mut store = ObjectStore::open(temporary.path().to_owned()).unwrap();
        store.ensure_workspace("workspace", b"/workspace").unwrap();
        let tree = empty_tree(&store);
        let snapshot = store
            .put_struct(ObjectKind::Snapshot, &snapshot(tree, None))
            .unwrap();
        fs::write(
            store.workspace_data_dir("workspace").join("restore.intent"),
            serde_json::to_vec(&serde_json::json!({
                "pre_snapshot": snapshot,
                "target_snapshot": snapshot,
                "paths": []
            }))
            .unwrap(),
        )
        .unwrap();

        let report = collect(&mut store, false).unwrap();
        assert_eq!(report.roots, 1);
        assert_eq!(report.reachable_objects, 2);
        assert!(store.read_bytes(&snapshot, ObjectKind::Snapshot).is_ok());
        assert!(store.read_bytes(&tree, ObjectKind::Tree).is_ok());
    }
}
