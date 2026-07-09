use crate::catalog::Catalog;
use crate::model::{ObjectId, ObjectKind};
use anyhow::Context;
use fs2::FileExt;
use serde::{de::DeserializeOwned, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const PACK_NAME: &str = "pack-000001.agp";

pub struct ObjectStore {
    root: PathBuf,
    catalog: Catalog,
}

impl ObjectStore {
    pub fn open(root: PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(root.join("packs"))?;
        fs::create_dir_all(root.join("locks"))?;
        let catalog = Catalog::open(&root.join("catalog.sqlite3"))?;
        Ok(Self { root, catalog })
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn catalog_mut(&mut self) -> &mut Catalog {
        &mut self.catalog
    }

    pub fn put_bytes(&self, kind: ObjectKind, bytes: &[u8]) -> anyhow::Result<ObjectId> {
        let id = object_id(kind, bytes);
        if self.catalog.object(&id)?.is_some() {
            return Ok(id);
        }

        let lock_path = self.root.join("locks").join("pack.lock");
        let lock = OpenOptions::new()
            .create(true)
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
            let offset = pack.seek(SeekFrom::End(0))?;
            pack.write_all(bytes)?;
            pack.sync_data()?;
            self.catalog
                .insert_object(&id, kind, PACK_NAME, offset, bytes.len() as u64)?;
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

    pub fn root(&self) -> &Path {
        &self.root
    }
}

pub fn object_id(kind: ObjectKind, bytes: &[u8]) -> ObjectId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(kind.domain());
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}
