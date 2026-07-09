use crate::model::{ObjectId, ObjectKind};
use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ObjectLocation {
    pub pack: String,
    pub offset: u64,
    pub len: u64,
    pub kind: ObjectKind,
}

pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn =
            Connection::open(path).with_context(|| format!("open catalog {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS objects (
                id BLOB PRIMARY KEY,
                kind INTEGER NOT NULL,
                pack TEXT NOT NULL,
                offset INTEGER NOT NULL,
                len INTEGER NOT NULL
            ) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS workspaces (
                id TEXT PRIMARY KEY,
                root BLOB NOT NULL UNIQUE,
                head BLOB
            );
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
            ",
        )?;
        Ok(Self { conn })
    }

    pub fn object(&self, id: &ObjectId) -> anyhow::Result<Option<ObjectLocation>> {
        self.conn
            .query_row(
                "SELECT kind, pack, offset, len FROM objects WHERE id = ?1",
                params![id.as_slice()],
                |row| {
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
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn insert_object(
        &self,
        id: &ObjectId,
        kind: ObjectKind,
        pack: &str,
        offset: u64,
        len: u64,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO objects(id, kind, pack, offset, len) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![id.as_slice(), kind as u8, pack, offset as i64, len as i64],
        )?;
        Ok(())
    }

    pub fn ensure_workspace(&self, id: &str, root: &[u8]) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO workspaces(id, root) VALUES(?1, ?2)",
            params![id, root],
        )?;
        Ok(())
    }

    pub fn workspace_head(&self, id: &str) -> anyhow::Result<Option<ObjectId>> {
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
        &mut self,
        workspace_id: &str,
        snapshot_id: &ObjectId,
        sealed_at: i64,
        label: Option<&str>,
        trigger: &str,
    ) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO timeline(workspace_id, snapshot_id, sealed_at, label, trigger)
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

fn vec_to_id(bytes: Vec<u8>) -> anyhow::Result<ObjectId> {
    anyhow::ensure!(bytes.len() == 32, "catalog contains an invalid object ID");
    let mut id = [0_u8; 32];
    id.copy_from_slice(&bytes);
    Ok(id)
}
