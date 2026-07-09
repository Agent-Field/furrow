use crate::model::{ObjectId, SnapshotTrigger};
use anyhow::Context;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"AGRF";
const MAX_RECORD: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefRecord {
    pub sequence: u64,
    pub snapshot_id: ObjectId,
    pub sealed_at: i64,
    pub label: Option<String>,
    pub trigger: SnapshotTrigger,
    pub previous_frame: ObjectId,
}

pub struct RefLog {
    path: PathBuf,
    lock_path: PathBuf,
}

impl RefLog {
    pub fn open(store_root: &Path, workspace_id: &str) -> anyhow::Result<Self> {
        let workspace_dir = store_root.join("workspaces").join(workspace_id);
        fs::create_dir_all(&workspace_dir)?;
        Ok(Self {
            path: workspace_dir.join("refs.log"),
            lock_path: workspace_dir.join("refs.lock"),
        })
    }

    pub fn records(&self) -> anyhow::Result<Vec<RefRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let mut file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        let (records, valid_len, incomplete_tail) = read_records(&mut file)?;
        if incomplete_tail {
            file.set_len(valid_len)?;
            file.sync_data()?;
        }
        Ok(records.into_iter().map(|(record, _)| record).collect())
    }

    pub fn append(
        &self,
        snapshot_id: ObjectId,
        sealed_at: i64,
        label: Option<String>,
        trigger: SnapshotTrigger,
    ) -> anyhow::Result<RefRecord> {
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&self.lock_path)?;
        lock.lock_exclusive()?;

        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.path)?;
        let (records, valid_len, incomplete_tail) = read_records(&mut file)?;
        if incomplete_tail {
            file.set_len(valid_len)?;
            file.seek(SeekFrom::End(0))?;
        }
        if let Some((existing, _)) = records
            .iter()
            .find(|(record, _)| record.snapshot_id == snapshot_id)
        {
            FileExt::unlock(&lock)?;
            return Ok(existing.clone());
        }

        let sequence = records.last().map_or(1, |(record, _)| record.sequence + 1);
        let previous_frame = records.last().map_or([0_u8; 32], |(_, hash)| *hash);
        let record = RefRecord {
            sequence,
            snapshot_id,
            sealed_at,
            label,
            trigger,
            previous_frame,
        };
        let payload = serde_json::to_vec(&record)?;
        anyhow::ensure!(payload.len() <= MAX_RECORD, "reference record is too large");
        let checksum = blake3::hash(&payload);
        file.write_all(MAGIC)?;
        file.write_all(&(payload.len() as u32).to_le_bytes())?;
        file.write_all(checksum.as_bytes())?;
        file.write_all(&payload)?;
        file.sync_data()?;
        FileExt::unlock(&lock)?;
        Ok(record)
    }
}

fn read_records(file: &mut File) -> anyhow::Result<(Vec<(RefRecord, ObjectId)>, u64, bool)> {
    file.seek(SeekFrom::Start(0))?;
    let file_len = file.metadata()?.len();
    let mut offset = 0_u64;
    let mut expected_previous = [0_u8; 32];
    let mut records = Vec::new();

    while offset < file_len {
        let frame_start = offset;
        let mut magic = [0_u8; 4];
        if let Err(error) = file.read_exact(&mut magic) {
            if error.kind() == ErrorKind::UnexpectedEof {
                return Ok((records, frame_start, true));
            }
            return Err(error.into());
        }
        anyhow::ensure!(
            &magic == MAGIC,
            "reference log corruption at byte {frame_start}"
        );
        let mut len_bytes = [0_u8; 4];
        if file.read_exact(&mut len_bytes).is_err() {
            return Ok((records, frame_start, true));
        }
        let len = u32::from_le_bytes(len_bytes) as usize;
        anyhow::ensure!(len <= MAX_RECORD, "reference record exceeds size limit");
        let mut checksum = [0_u8; 32];
        if file.read_exact(&mut checksum).is_err() {
            return Ok((records, frame_start, true));
        }
        let mut payload = vec![0_u8; len];
        if file.read_exact(&mut payload).is_err() {
            return Ok((records, frame_start, true));
        }
        anyhow::ensure!(
            blake3::hash(&payload).as_bytes() == &checksum,
            "reference checksum mismatch"
        );
        let record: RefRecord =
            serde_json::from_slice(&payload).context("decode reference record")?;
        anyhow::ensure!(
            record.previous_frame == expected_previous,
            "broken reference hash chain"
        );
        expected_previous =
            *blake3::hash(&[magic.as_slice(), &len_bytes, &checksum, &payload].concat()).as_bytes();
        offset = file.stream_position()?;
        records.push((record, expected_previous));
    }
    Ok((records, offset, false))
}
