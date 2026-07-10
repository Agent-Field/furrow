//! Encrypted, delta-only synchronization through a developer-owned directory.

use crate::bundle;
use crate::model::{ObjectId, ObjectKind, Snapshot};
use crate::remote_crypto::RemoteCrypto;
use crate::store::ObjectStore;
use anyhow::Context;
use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LEASE_SECONDS: u64 = 60 * 60;
const MAX_REMOTE_OBJECT_BYTES: u64 = 256 * 1024 * 1024 + 1024;
const MAX_REMOTE_METADATA_BYTES: u64 = 1024 * 1024 + 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairConfig {
    pub remote: PathBuf,
    pub namespace: String,
    pub key: [u8; 32],
    pub machine_id: [u8; 16],
}

#[derive(Debug, Clone, Serialize)]
pub struct PairSummary {
    pub remote: PathBuf,
    pub namespace: String,
    pub key_hex: String,
    pub machine_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PushReport {
    pub snapshot: String,
    pub root: String,
    pub objects: u64,
    pub uploaded_objects: u64,
    pub reused_objects: u64,
    pub uploaded_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PullReport {
    pub snapshot: String,
    pub fetched_objects: u64,
    pub reused_objects: u64,
    pub fetched_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteHead {
    version: u8,
    snapshot: ObjectId,
    root: ObjectId,
    base_root: Option<ObjectId>,
    publisher: [u8; 16],
}

#[derive(Debug, Clone)]
pub struct PulledHead {
    pub snapshot: ObjectId,
    pub root: ObjectId,
    pub base_root: Option<ObjectId>,
    pub report: PullReport,
}

pub fn pair(
    config_path: &Path,
    remote: &Path,
    namespace: &str,
    key_hex: Option<&str>,
) -> anyhow::Result<PairSummary> {
    validate_namespace(namespace)?;
    let requested_remote = if remote.is_absolute() {
        remote.to_owned()
    } else {
        std::env::current_dir()?.join(remote)
    };
    ensure_durable_directory(&requested_remote)?;
    let remote = requested_remote
        .canonicalize()
        .with_context(|| format!("resolve sync remote {}", requested_remote.display()))?;
    let key = match key_hex {
        Some(key) => parse_key(key)?,
        None => RemoteCrypto::generate_key()?,
    };
    let mut machine_id = [0; 16];
    getrandom::getrandom(&mut machine_id)
        .map_err(|error| anyhow::anyhow!("generate machine ID: {error}"))?;
    let config = PairConfig {
        remote: remote.clone(),
        namespace: namespace.to_owned(),
        key,
        machine_id,
    };
    let parent = config_path.parent().context("sync config has no parent")?;
    ensure_durable_directory(parent)?;
    atomic_write(config_path, &serde_json::to_vec_pretty(&config)?)?;
    fs::set_permissions(config_path, fs::Permissions::from_mode(0o600))?;
    ensure_durable_directory(&remote_root(&config).join("objects"))?;
    Ok(PairSummary {
        remote,
        namespace: namespace.to_owned(),
        key_hex: hex::encode(key),
        machine_id: hex::encode(machine_id),
    })
}

pub fn load(config_path: &Path) -> anyhow::Result<PairConfig> {
    serde_json::from_slice(&fs::read(config_path).with_context(|| {
        format!(
            "repository is not paired; run `agit pair <directory>` first ({})",
            config_path.display()
        )
    })?)
    .context("decode sync configuration")
}

pub fn push(
    store: &ObjectStore,
    snapshot: ObjectId,
    config: &PairConfig,
    expected_remote_root: Option<ObjectId>,
    takeover: bool,
) -> anyhow::Result<PushReport> {
    let root = remote_root(config);
    ensure_durable_directory(&root.join("objects"))?;
    let _lease = acquire_writer_lease(config, takeover)?;
    let crypto = RemoteCrypto::new(config.key);
    let snapshot_value: Snapshot = store.read_struct(&snapshot, ObjectKind::Snapshot)?;
    let base_root = if root.join("HEAD").exists() {
        let current = read_remote_head(&root, config, &crypto)?;
        anyhow::ensure!(
            current.snapshot == snapshot || Some(current.root) == expected_remote_root,
            "remote workspace changed since this machine last synchronized; pull before pushing"
        );
        Some(current.root)
    } else {
        anyhow::ensure!(
            expected_remote_root.is_none(),
            "paired remote lost its published head; refusing to recreate it without re-pairing"
        );
        None
    };
    let mut uploaded_objects = 0_u64;
    let mut reused_objects = 0_u64;
    let mut uploaded_bytes = 0_u64;
    let objects = bundle::for_each_reachable(store, snapshot, |id, kind, bytes| {
        let path = remote_object_path(&root, &crypto.remote_id(id));
        if path.exists() {
            reused_objects += 1;
            return Ok(());
        }
        let encrypted = crypto.encrypt_object(id, kind, bytes)?;
        atomic_write(&path, &encrypted)?;
        uploaded_objects += 1;
        uploaded_bytes += encrypted.len() as u64;
        Ok(())
    })?;
    let head = RemoteHead {
        version: 1,
        snapshot,
        root: snapshot_value.root_tree,
        base_root,
        publisher: config.machine_id,
    };
    let encrypted_head = crypto.encrypt_metadata(
        &serde_json::to_vec(&head)?,
        head_context(&config.namespace).as_bytes(),
    )?;
    atomic_write(&root.join("HEAD"), &encrypted_head)?;
    Ok(PushReport {
        snapshot: hex::encode(snapshot),
        root: hex::encode(snapshot_value.root_tree),
        objects,
        uploaded_objects,
        reused_objects,
        uploaded_bytes,
    })
}

pub fn pull(store: &ObjectStore, config: &PairConfig) -> anyhow::Result<PulledHead> {
    let root = remote_root(config);
    let crypto = RemoteCrypto::new(config.key);
    let head = read_remote_head(&root, config, &crypto)?;
    let queue_file = tempfile::NamedTempFile::new()?;
    let queue = Connection::open(queue_file.path())?;
    queue.execute_batch(
        "PRAGMA journal_mode=OFF;
         PRAGMA synchronous=OFF;
         CREATE TABLE queue(
            id BLOB PRIMARY KEY,
            kind INTEGER NOT NULL,
            processed INTEGER NOT NULL DEFAULT 0
         ) WITHOUT ROWID;
         CREATE INDEX queue_pending ON queue(processed, id);
         BEGIN;",
    )?;
    enqueue(&queue, &head.snapshot, ObjectKind::Snapshot)?;
    let mut fetched_objects = 0_u64;
    let mut reused_objects = 0_u64;
    let mut fetched_bytes = 0_u64;
    while let Some((id, expected)) = next(&queue)? {
        let bytes = if store.contains_object(&id)? {
            reused_objects += 1;
            store.read_bytes(&id, expected)?
        } else {
            let encrypted = read_bounded(
                &remote_object_path(&root, &crypto.remote_id(&id)),
                MAX_REMOTE_OBJECT_BYTES,
            )
            .with_context(|| format!("remote is missing object {}", hex::encode(id)))?;
            let (kind, bytes) = crypto.decrypt_object(&id, &encrypted)?;
            anyhow::ensure!(kind == expected, "remote object kind mismatch");
            anyhow::ensure!(
                store.put_bytes(kind, &bytes)? == id,
                "remote import ID mismatch"
            );
            fetched_objects += 1;
            fetched_bytes += encrypted.len() as u64;
            bytes
        };
        for (child, child_kind) in bundle::object_edges(expected, &bytes)? {
            enqueue(&queue, &child, child_kind)?;
        }
        queue.execute(
            "UPDATE queue SET processed = 1 WHERE id = ?1",
            params![id.as_slice()],
        )?;
    }
    queue.execute_batch("COMMIT;")?;
    // Existing objects are trusted only because the local completeness gate
    // made them visible. A final traversal proves the assembled remote root.
    bundle::for_each_reachable(store, head.snapshot, |_id, _kind, _bytes| Ok(()))?;
    let remote_snapshot: Snapshot = store.read_struct(&head.snapshot, ObjectKind::Snapshot)?;
    anyhow::ensure!(
        remote_snapshot.root_tree == head.root,
        "authenticated remote head does not match its snapshot root"
    );
    Ok(PulledHead {
        snapshot: head.snapshot,
        root: head.root,
        base_root: head.base_root,
        report: PullReport {
            snapshot: hex::encode(head.snapshot),
            fetched_objects,
            reused_objects,
            fetched_bytes,
        },
    })
}

fn acquire_writer_lease(config: &PairConfig, takeover: bool) -> anyhow::Result<LeaseLock> {
    let root = remote_root(config);
    ensure_durable_directory(&root)?;
    let lock_path = root.join("LEASE.lock");
    let lock = LeaseLock::acquire(&lock_path)?;
    let crypto = RemoteCrypto::new(config.key);
    let lease_path = root.join("LEASE");
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    if lease_path.exists() {
        let lease = crypto.decrypt_head(
            &read_bounded(&lease_path, MAX_REMOTE_METADATA_BYTES)?,
            b"agit:writer-lease:v1",
        )?;
        let mut owner = [0; 16];
        owner.copy_from_slice(&lease[..16]);
        let expires = u64::from_le_bytes(lease[16..24].try_into().unwrap());
        anyhow::ensure!(
            owner == config.machine_id || expires <= now || takeover,
            "another machine holds the writer lease until {expires}; use --takeover only after confirming it is offline"
        );
    }
    let mut lease = [0; 32];
    lease[..16].copy_from_slice(&config.machine_id);
    lease[16..24].copy_from_slice(&(now + LEASE_SECONDS).to_le_bytes());
    let encrypted = crypto.encrypt_head(&lease, b"agit:writer-lease:v1")?;
    atomic_write(&lease_path, &encrypted)?;
    Ok(lock)
}

struct LeaseLock {
    file: fs::File,
}

impl LeaseLock {
    fn acquire(path: &Path) -> anyhow::Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.try_lock_exclusive()
            .context("another sync operation is updating the remote")?;
        file.set_len(0)?;
        writeln!(file, "{}", std::process::id())?;
        file.sync_all()?;
        Ok(Self { file })
    }
}

impl Drop for LeaseLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn remote_root(config: &PairConfig) -> PathBuf {
    config.remote.join(&config.namespace)
}

fn head_context(namespace: &str) -> String {
    format!("agit:remote-head:v1:{namespace}")
}

fn read_remote_head(
    root: &Path,
    config: &PairConfig,
    crypto: &RemoteCrypto,
) -> anyhow::Result<RemoteHead> {
    let encrypted = read_bounded(&root.join("HEAD"), MAX_REMOTE_METADATA_BYTES)
        .context("sync remote has no published HEAD")?;
    let head: RemoteHead = serde_json::from_slice(
        &crypto.decrypt_metadata(&encrypted, head_context(&config.namespace).as_bytes())?,
    )
    .context("decode authenticated remote head")?;
    anyhow::ensure!(head.version == 1, "unsupported remote head version");
    Ok(head)
}

fn read_bounded(path: &Path, limit: u64) -> anyhow::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    anyhow::ensure!(length <= limit, "remote file exceeds its size limit");
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(limit + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= limit,
        "remote file exceeds its size limit"
    );
    Ok(bytes)
}

fn remote_object_path(root: &Path, id: &ObjectId) -> PathBuf {
    let hex = hex::encode(id);
    root.join("objects").join(&hex[..2]).join(&hex[2..])
}

fn validate_namespace(namespace: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !namespace.is_empty() && namespace.len() <= 96,
        "invalid sync namespace"
    );
    anyhow::ensure!(
        namespace
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "sync namespace may contain only letters, numbers, dot, dash, and underscore"
    );
    anyhow::ensure!(
        namespace != "." && namespace != "..",
        "invalid sync namespace"
    );
    Ok(())
}

fn parse_key(value: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = hex::decode(value).context("sync key must be hexadecimal")?;
    anyhow::ensure!(bytes.len() == 32, "sync key must contain 64 hex characters");
    let mut key = [0; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().context("remote object has no parent")?;
    ensure_durable_directory(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn ensure_durable_directory(path: &Path) -> anyhow::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    let parent = path.parent().context("directory has no parent")?;
    ensure_durable_directory(parent)?;
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
        Err(error) => return Err(error.into()),
    }
    fs::File::open(path)?.sync_all()?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn enqueue(connection: &Connection, id: &ObjectId, kind: ObjectKind) -> anyhow::Result<()> {
    connection.execute(
        "INSERT OR IGNORE INTO queue(id, kind) VALUES(?1, ?2)",
        params![id.as_slice(), kind as u8],
    )?;
    let actual: u8 = connection.query_row(
        "SELECT kind FROM queue WHERE id = ?1",
        params![id.as_slice()],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        actual == kind as u8,
        "object reached with conflicting kinds"
    );
    Ok(())
}

fn next(connection: &Connection) -> anyhow::Result<Option<(ObjectId, ObjectKind)>> {
    let raw: Option<(Vec<u8>, u8)> = connection
        .query_row(
            "SELECT id, kind FROM queue WHERE processed = 0 ORDER BY id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    raw.map(|(bytes, kind)| {
        anyhow::ensure!(bytes.len() == 32, "invalid queued object ID");
        let mut id = [0; 32];
        id.copy_from_slice(&bytes);
        Ok((
            id,
            ObjectKind::from_u8(kind).context("invalid queued object kind")?,
        ))
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Blob, ChunkRef, EntryKind, SealQuality, SnapshotTrigger, Tree, TreeEntry};

    fn snapshot_fixture(store: &ObjectStore) -> ObjectId {
        let chunk = store
            .put_bytes(ObjectKind::Chunk, b"resumable remote data")
            .unwrap();
        let blob = store
            .put_struct(
                ObjectKind::Blob,
                &Blob {
                    chunks: vec![ChunkRef { id: chunk, len: 21 }],
                    total_len: 21,
                },
            )
            .unwrap();
        let tree = store
            .put_struct(
                ObjectKind::Tree,
                &Tree {
                    entries: vec![TreeEntry {
                        name: b"state.txt".to_vec(),
                        kind: EntryKind::File,
                        target: Some(blob),
                        link_target: Vec::new(),
                        mode: 0o100644,
                        size: 21,
                        mtime_secs: 0,
                        mtime_nanos: 0,
                        xattrs: None,
                    }],
                    pages: Vec::new(),
                },
            )
            .unwrap();
        store
            .put_struct(
                ObjectKind::Snapshot,
                &Snapshot {
                    root_tree: tree,
                    parent: None,
                    merge_parents: Vec::new(),
                    sealed_at_secs: 0,
                    sealed_at_nanos: 0,
                    quality: SealQuality::Quiescent,
                    trigger: SnapshotTrigger::Manual,
                    label: None,
                    sqlite_backups: Vec::new(),
                },
            )
            .unwrap()
    }

    #[test]
    fn pair_config_is_private_and_lease_rejects_a_second_writer() {
        let temporary = tempfile::tempdir().unwrap();
        let config_one = temporary.path().join("one.json");
        let remote = temporary.path().join("remote");
        let first = pair(&config_one, &remote, "project", None).unwrap();
        assert_eq!(
            fs::metadata(&config_one).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let first_config = load(&config_one).unwrap();
        acquire_writer_lease(&first_config, false).unwrap();

        let config_two = temporary.path().join("two.json");
        pair(&config_two, &remote, "project", Some(&first.key_hex)).unwrap();
        let second_config = load(&config_two).unwrap();
        assert!(acquire_writer_lease(&second_config, false).is_err());
        acquire_writer_lease(&second_config, true).unwrap();
    }

    #[test]
    fn pull_resumes_when_an_existing_snapshot_is_missing_descendants() {
        let temporary = tempfile::tempdir().unwrap();
        let source = ObjectStore::open(temporary.path().join("source-store")).unwrap();
        let snapshot = snapshot_fixture(&source);
        let config_path = temporary.path().join("pair.json");
        let remote = temporary.path().join("remote");
        pair(&config_path, &remote, "project", None).unwrap();
        let config = load(&config_path).unwrap();
        push(&source, snapshot, &config, None, false).unwrap();

        let destination = ObjectStore::open(temporary.path().join("destination-store")).unwrap();
        let snapshot_bytes = source.read_bytes(&snapshot, ObjectKind::Snapshot).unwrap();
        assert_eq!(
            destination
                .put_bytes(ObjectKind::Snapshot, &snapshot_bytes)
                .unwrap(),
            snapshot
        );
        let pulled = pull(&destination, &config).unwrap();
        assert_eq!(pulled.snapshot, snapshot);
        assert_eq!(pulled.report.reused_objects, 1);
        assert_eq!(pulled.report.fetched_objects, 3);
        assert_eq!(
            bundle::for_each_reachable(&destination, snapshot, |_, _, _| Ok(())).unwrap(),
            4
        );
    }
}
