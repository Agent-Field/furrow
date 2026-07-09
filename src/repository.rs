use crate::catalog::CachedFile;
use crate::chunker::ChunkStream;
use crate::model::{
    id_hex, parse_id, Blob, ChunkRef, EntryKind, ObjectId, ObjectKind, SealQuality, Snapshot,
    SnapshotTrigger, SqliteBackup, Tree, TreeEntry, XattrEntry, Xattrs,
};
use crate::sqlite_adapter;
use crate::store::ObjectStore;
use anyhow::{bail, Context};
use directories::ProjectDirs;
use filetime::FileTime;
use fs2::FileExt;
use serde::Serialize;
use serde::{Deserialize, Serialize as SerdeSerialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const WORKSPACE_FILE: &str = ".agit/workspace-id";

pub struct AgitRepository {
    root: PathBuf,
    workspace_id: String,
    store: ObjectStore,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotSummary {
    pub id: String,
    pub sealed_at: i64,
    pub label: Option<String>,
    pub trigger: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RewindChange {
    pub path: String,
    pub action: &'static str,
    #[serde(skip)]
    raw_path: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RewindPlan {
    pub target: String,
    pub changes: Vec<RewindChange>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepositoryStatus {
    pub workspace: PathBuf,
    pub store: PathBuf,
    pub head: Option<String>,
    pub snapshots: usize,
    pub objects: u64,
    pub physical_bytes: u64,
    pub watcher_running: bool,
}

#[derive(Clone)]
struct FlatEntry {
    entry: TreeEntry,
}

#[derive(Debug, SerdeSerialize, Deserialize)]
struct RestoreIntent {
    pre_snapshot: ObjectId,
    target_snapshot: ObjectId,
    paths: Vec<Vec<u8>>,
}

impl AgitRepository {
    pub fn watch(root: &Path) -> anyhow::Result<(Self, ObjectId)> {
        let root = root
            .canonicalize()
            .with_context(|| format!("open {}", root.display()))?;
        anyhow::ensure!(
            root.join(".git").exists(),
            "agit currently requires a Git repository"
        );
        fs::create_dir_all(root.join(".agit"))?;
        let workspace_path = root.join(WORKSPACE_FILE);
        let workspace_id = if workspace_path.exists() {
            fs::read_to_string(&workspace_path)?.trim().to_owned()
        } else {
            let id = new_workspace_id(&root);
            atomic_write(&workspace_path, format!("{id}\n").as_bytes())?;
            id
        };

        let store_root = data_root()?.join("store-v1");
        anyhow::ensure!(
            !root.starts_with(&store_root),
            "workspace cannot contain the agit store"
        );
        let mut store = ObjectStore::open(store_root)?;
        store.ensure_workspace(&workspace_id, root.as_os_str().as_bytes())?;
        let mut repository = Self {
            root,
            workspace_id,
            store,
        };
        repository.recover_interrupted_rewind()?;
        let trigger = if repository
            .store
            .workspace_head(&repository.workspace_id)?
            .is_some()
        {
            SnapshotTrigger::Manual
        } else {
            SnapshotTrigger::Initial
        };
        let id = repository.snapshot(Some("initial protection".to_owned()), trigger)?;
        Ok((repository, id))
    }

    pub fn open(root: &Path) -> anyhow::Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("open {}", root.display()))?;
        let mut store = ObjectStore::open(data_root()?.join("store-v1"))?;
        let workspace_path = root.join(WORKSPACE_FILE);
        let workspace_id = if workspace_path.exists() {
            fs::read_to_string(&workspace_path)?.trim().to_owned()
        } else {
            let id = store
                .find_workspace(root.as_os_str().as_bytes())?
                .context("this repository is not watched; run `agit watch` first")?;
            fs::create_dir_all(root.join(".agit"))?;
            atomic_write(&workspace_path, format!("{id}\n").as_bytes())?;
            id
        };
        store.ensure_workspace(&workspace_id, root.as_os_str().as_bytes())?;
        let repository = Self {
            root,
            workspace_id,
            store,
        };
        repository.recover_interrupted_rewind()?;
        Ok(repository)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn store_root(&self) -> &Path {
        self.store.root()
    }

    pub fn workspace_data_dir(&self) -> PathBuf {
        self.store.workspace_data_dir(&self.workspace_id)
    }

    pub fn snapshot(
        &mut self,
        label: Option<String>,
        trigger: SnapshotTrigger,
    ) -> anyhow::Result<ObjectId> {
        let parent = self.store.workspace_head(&self.workspace_id)?;
        let root_tree = self.capture_root_retry()?;
        let sqlite_backups = self.capture_sqlite_backups()?;
        let (secs, nanos) = now();
        let snapshot = Snapshot {
            root_tree,
            parent,
            sealed_at_secs: secs,
            sealed_at_nanos: nanos,
            quality: SealQuality::Quiescent,
            trigger: trigger.clone(),
            label: label.clone(),
            sqlite_backups,
        };
        let id = self.store.put_struct(ObjectKind::Snapshot, &snapshot)?;
        self.store
            .publish_snapshot(&self.workspace_id, id, secs, label, trigger)?;
        Ok(id)
    }

    pub fn timeline(&self, limit: usize) -> anyhow::Result<Vec<SnapshotSummary>> {
        self.store
            .timeline(&self.workspace_id, limit)?
            .into_iter()
            .map(|row| {
                Ok(SnapshotSummary {
                    id: id_hex(&row.id),
                    sealed_at: row.sealed_at,
                    label: row.label,
                    trigger: row.trigger,
                })
            })
            .collect()
    }

    pub fn status(&self) -> anyhow::Result<RepositoryStatus> {
        let timeline = self.timeline(100_000)?;
        let stats = self.store.stats()?;
        Ok(RepositoryStatus {
            workspace: self.root.clone(),
            store: self.store.root().to_owned(),
            head: timeline.first().map(|item| item.id.clone()),
            snapshots: timeline.len(),
            objects: stats.objects,
            physical_bytes: stats.physical_bytes,
            watcher_running: self.watcher_running(),
        })
    }

    pub fn resolve_snapshot(&self, value: &str) -> anyhow::Result<ObjectId> {
        if value.len() == 64 {
            let id = parse_id(value)?;
            self.store.read_bytes(&id, ObjectKind::Snapshot)?;
            return Ok(id);
        }
        anyhow::ensure!(
            value.len() >= 8,
            "snapshot prefix must contain at least 8 characters"
        );
        let matches: Vec<ObjectId> = self
            .store
            .timeline(&self.workspace_id, 100_000)?
            .into_iter()
            .filter(|row| id_hex(&row.id).starts_with(value))
            .map(|row| row.id)
            .collect();
        match matches.as_slice() {
            [id] => Ok(*id),
            [] => bail!("snapshot `{value}` was not found"),
            _ => bail!("snapshot prefix `{value}` is ambiguous"),
        }
    }

    pub fn plan_rewind(&self, target: &ObjectId, paths: &[PathBuf]) -> anyhow::Result<RewindPlan> {
        let target_snapshot: Snapshot = self.store.read_struct(target, ObjectKind::Snapshot)?;
        let target_entries = self.flatten_tree(&target_snapshot.root_tree)?;
        let current_tree = self.capture_root_retry()?;
        let current_entries = self.flatten_tree(&current_tree)?;
        let mut all_paths = BTreeSet::new();
        all_paths.extend(target_entries.keys().cloned());
        all_paths.extend(current_entries.keys().cloned());

        let mut changes = Vec::new();
        for path in all_paths {
            if !selected(&path, paths) {
                continue;
            }
            let before = current_entries.get(&path).map(|entry| &entry.entry);
            let after = target_entries.get(&path).map(|entry| &entry.entry);
            if before == after {
                continue;
            }
            let action = match (before, after) {
                (None, Some(_)) => "restore",
                (Some(_), None) => "remove",
                (Some(_), Some(_)) => "replace",
                (None, None) => unreachable!(),
            };
            changes.push(RewindChange {
                path: display_relative(&path),
                action,
                raw_path: path,
            });
        }
        Ok(RewindPlan {
            target: id_hex(target),
            changes,
        })
    }

    pub fn rewind(
        &mut self,
        target: &ObjectId,
        paths: &[PathBuf],
        sqlite_consistent: bool,
    ) -> anyhow::Result<(ObjectId, RewindPlan)> {
        let lock = self.acquire_mutation_lock()?;
        let plan = self.plan_rewind(target, paths)?;
        let pre = self.snapshot(
            Some(format!("before rewind to {}", &id_hex(target)[..12])),
            SnapshotTrigger::PreRewind,
        )?;
        let target_snapshot: Snapshot = self.store.read_struct(target, ObjectKind::Snapshot)?;
        let target_entries = self.flatten_tree(&target_snapshot.root_tree)?;
        self.write_restore_intent(&RestoreIntent {
            pre_snapshot: pre,
            target_snapshot: *target,
            paths: paths
                .iter()
                .map(|path| path.as_os_str().as_bytes().to_vec())
                .collect(),
        })?;

        let result = self
            .apply_rewind(&target_entries, &plan, paths)
            .and_then(|_| {
                if sqlite_consistent {
                    self.restore_sqlite_backups(&target_snapshot.sqlite_backups, paths)?;
                }
                Ok(())
            });
        if let Err(error) = result {
            let pre_snapshot: Snapshot = self.store.read_struct(&pre, ObjectKind::Snapshot)?;
            let pre_entries = self.flatten_tree(&pre_snapshot.root_tree)?;
            let rollback_plan = self.plan_rewind(&pre, paths)?;
            self.apply_rewind(&pre_entries, &rollback_plan, paths)
                .context("rewind failed and rollback also failed")?;
            self.clear_restore_intent()?;
            FileExt::unlock(&lock)?;
            return Err(error.context("rewind aborted; the pre-rewind state was restored"));
        }
        self.clear_restore_intent()?;
        FileExt::unlock(&lock)?;
        Ok((pre, plan))
    }

    pub fn forget(self, purge: bool) -> anyhow::Result<()> {
        self.stop_watcher()?;
        let workspace_file = self.root.join(WORKSPACE_FILE);
        if workspace_file.exists() {
            fs::remove_file(workspace_file)?;
        }
        if purge {
            // Reachability-aware physical collection is intentionally performed by GC.
            eprintln!("workspace detached; unreachable data will be removed by `agit gc`");
        }
        Ok(())
    }

    fn watcher_running(&self) -> bool {
        let pid_path = self.workspace_data_dir().join("daemon.pid");
        let Ok(pid) = fs::read_to_string(pid_path) else {
            return false;
        };
        let Ok(pid) = pid.trim().parse::<i32>() else {
            return false;
        };
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn stop_watcher(&self) -> anyhow::Result<()> {
        let pid_path = self.workspace_data_dir().join("daemon.pid");
        if let Ok(pid) = fs::read_to_string(&pid_path) {
            if let Ok(pid) = pid.trim().parse::<i32>() {
                let result = unsafe { libc::kill(pid, libc::SIGTERM) };
                if result != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        return Err(error.into());
                    }
                }
            }
        }
        if pid_path.exists() {
            fs::remove_file(pid_path)?;
        }
        Ok(())
    }

    fn capture_directory(&self, path: &Path) -> anyhow::Result<ObjectId> {
        self.capture_directory_impl(path, true)
    }

    fn capture_root_retry(&self) -> anyhow::Result<ObjectId> {
        let mut last_error = None;
        for _ in 0..3 {
            match self.capture_directory(&self.root) {
                Ok(id) => return Ok(id),
                Err(error) if is_not_found(&error) => last_error = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last_error
            .context("capture retry failed without an error")?
            .context("workspace kept changing while it was captured"))
    }

    fn capture_directory_impl(&self, path: &Path, _publication: bool) -> anyhow::Result<ObjectId> {
        let mut entries = Vec::new();
        let mut children: Vec<_> = fs::read_dir(path)
            .with_context(|| format!("read directory {}", path.display()))?
            .collect::<Result<_, _>>()?;
        children.sort_by(|a, b| a.file_name().as_bytes().cmp(b.file_name().as_bytes()));

        for child in children {
            let child_path = child.path();
            if child_path.starts_with(self.store.root()) {
                continue;
            }
            let metadata = fs::symlink_metadata(&child_path)
                .with_context(|| format!("stat {}", child_path.display()))?;
            let file_type = metadata.file_type();
            let (secs, nanos) = metadata_time(&metadata);
            let name = child.file_name().as_bytes().to_vec();
            let mode = metadata.permissions().mode();
            let xattrs = if file_type.is_file() || file_type.is_dir() {
                self.capture_xattrs(&child_path)
                    .with_context(|| format!("capture xattrs for {}", child_path.display()))?
            } else {
                None
            };

            let entry = if file_type.is_dir() {
                let target = self
                    .capture_directory_impl(&child_path, _publication)
                    .with_context(|| format!("capture directory {}", child_path.display()))?;
                TreeEntry {
                    name,
                    kind: EntryKind::Directory,
                    target: Some(target),
                    link_target: Vec::new(),
                    mode,
                    size: 0,
                    mtime_secs: secs,
                    mtime_nanos: nanos,
                    xattrs,
                }
            } else if file_type.is_file() {
                let relative = child_path.strip_prefix(&self.root)?.as_os_str().as_bytes();
                let target = self
                    .capture_file(&child_path, Some(relative))
                    .with_context(|| format!("capture file {}", child_path.display()))?;
                TreeEntry {
                    name,
                    kind: EntryKind::File,
                    target: Some(target),
                    link_target: Vec::new(),
                    mode,
                    size: metadata.len(),
                    mtime_secs: secs,
                    mtime_nanos: nanos,
                    xattrs,
                }
            } else if file_type.is_symlink() {
                TreeEntry {
                    name,
                    kind: EntryKind::Symlink,
                    target: None,
                    link_target: fs::read_link(&child_path)?.as_os_str().as_bytes().to_vec(),
                    mode,
                    size: metadata.len(),
                    mtime_secs: secs,
                    mtime_nanos: nanos,
                    xattrs: None,
                }
            } else if file_type.is_fifo() {
                special_entry(name, EntryKind::Fifo, mode, secs, nanos)
            } else if file_type.is_socket() {
                special_entry(name, EntryKind::SocketMarker, mode, secs, nanos)
            } else {
                eprintln!(
                    "warning: unsupported special file skipped: {}",
                    child_path.display()
                );
                continue;
            };
            entries.push(entry);
        }
        let tree = Tree { entries };
        self.store.put_struct(ObjectKind::Tree, &tree)
    }

    fn capture_file(&self, path: &Path, cache_key: Option<&[u8]>) -> anyhow::Result<ObjectId> {
        for _ in 0..3 {
            let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
            let before = file
                .metadata()
                .with_context(|| format!("stat open file {}", path.display()))?;
            if let Some(key) = cache_key {
                if let Some(cached) = self.store.cached_file(&self.workspace_id, key)? {
                    if cached_matches(&cached, &before) {
                        return Ok(cached.blob_id);
                    }
                }
            }
            let mut stream = ChunkStream::new(file);
            let mut chunks = Vec::new();
            let mut total_len = 0_u64;
            while let Some(chunk) = stream.next_chunk()? {
                let id = self.store.put_bytes(ObjectKind::Chunk, &chunk)?;
                total_len += chunk.len() as u64;
                chunks.push(ChunkRef {
                    id,
                    len: chunk.len() as u32,
                });
            }
            let after = fs::metadata(path).with_context(|| format!("restat {}", path.display()))?;
            if stable_metadata(&before, &after) {
                let blob_id = self
                    .store
                    .put_struct(ObjectKind::Blob, &Blob { chunks, total_len })?;
                if let Some(key) = cache_key {
                    self.store.cache_file(
                        &self.workspace_id,
                        key,
                        &cached_from_metadata(&after, blob_id),
                    )?;
                }
                return Ok(blob_id);
            }
        }
        bail!(
            "file changed repeatedly while being captured: {}",
            path.display()
        )
    }

    fn capture_xattrs(&self, path: &Path) -> anyhow::Result<Option<ObjectId>> {
        let mut entries = Vec::new();
        match xattr::list(path) {
            Ok(names) => {
                for name in names {
                    if let Some(value) = xattr::get(path, &name)
                        .with_context(|| format!("read xattr {:?} on {}", name, path.display()))?
                    {
                        entries.push(XattrEntry {
                            name: name.as_os_str().as_bytes().to_vec(),
                            value,
                        });
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        if entries.is_empty() {
            Ok(None)
        } else {
            Ok(Some(
                self.store
                    .put_struct(ObjectKind::Xattrs, &Xattrs { entries })?,
            ))
        }
    }

    fn capture_sqlite_backups(&self) -> anyhow::Result<Vec<SqliteBackup>> {
        let mut candidates = Vec::new();
        collect_sqlite_candidates(&self.root, &mut candidates)?;
        let mut backups = Vec::new();
        let temp_dir = self.store.root().join("tmp");
        for path in candidates {
            match sqlite_adapter::consistent_backup(&path, &temp_dir) {
                Ok(backup) => {
                    let blob = self.capture_file(backup.file.path(), None)?;
                    let relative = path
                        .strip_prefix(&self.root)?
                        .as_os_str()
                        .as_bytes()
                        .to_vec();
                    backups.push(SqliteBackup {
                        path: relative,
                        blob,
                        integrity_ok: backup.integrity_ok,
                    });
                }
                Err(error) => {
                    eprintln!(
                        "warning: SQLite consistent backup unavailable for {}: {error:#}; raw bytes remain protected",
                        path.display()
                    );
                }
            }
        }
        backups.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(backups)
    }

    fn restore_sqlite_backups(
        &self,
        backups: &[SqliteBackup],
        selected_paths: &[PathBuf],
    ) -> anyhow::Result<()> {
        for backup in backups {
            if !selected(&backup.path, selected_paths) {
                continue;
            }
            anyhow::ensure!(
                backup.integrity_ok,
                "refusing a SQLite backup that failed integrity_check"
            );
            let destination = safe_join(&self.root, &backup.path)?;
            let entry = TreeEntry {
                name: destination
                    .file_name()
                    .context("SQLite path has no filename")?
                    .as_bytes()
                    .to_vec(),
                kind: EntryKind::File,
                target: Some(backup.blob),
                link_target: Vec::new(),
                mode: 0o100600,
                size: 0,
                mtime_secs: now().0,
                mtime_nanos: 0,
                xattrs: None,
            };
            self.restore_file(&destination, &entry)?;
            let path_bytes = destination.as_os_str().as_bytes();
            for suffix in [b"-wal".as_slice(), b"-shm".as_slice()] {
                let mut sidecar = path_bytes.to_vec();
                sidecar.extend_from_slice(suffix);
                let sidecar = PathBuf::from(OsString::from_vec(sidecar));
                if sidecar.exists() {
                    fs::remove_file(sidecar)?;
                }
            }
        }
        Ok(())
    }

    fn flatten_tree(&self, root: &ObjectId) -> anyhow::Result<BTreeMap<Vec<u8>, FlatEntry>> {
        let mut result = BTreeMap::new();
        self.flatten_into(root, Vec::new(), &mut result)?;
        Ok(result)
    }

    fn flatten_into(
        &self,
        tree_id: &ObjectId,
        prefix: Vec<u8>,
        output: &mut BTreeMap<Vec<u8>, FlatEntry>,
    ) -> anyhow::Result<()> {
        let tree: Tree = self.store.read_struct(tree_id, ObjectKind::Tree)?;
        for entry in tree.entries {
            validate_name(&entry.name)?;
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(&entry.name);
            output.insert(
                path.clone(),
                FlatEntry {
                    entry: entry.clone(),
                },
            );
            if entry.kind == EntryKind::Directory {
                self.flatten_into(
                    &entry.target.context("directory missing tree ID")?,
                    path,
                    output,
                )?;
            }
        }
        Ok(())
    }

    fn apply_rewind(
        &self,
        target: &BTreeMap<Vec<u8>, FlatEntry>,
        plan: &RewindPlan,
        selected_paths: &[PathBuf],
    ) -> anyhow::Result<()> {
        let mut applied_operations = 0_usize;
        let changed: BTreeSet<Vec<u8>> = plan
            .changes
            .iter()
            .map(|change| change.raw_path.clone())
            .collect();

        // Directories must exist before files are written.
        for (path, flat) in target {
            if !changed.contains(path)
                || !selected(path, selected_paths)
                || flat.entry.kind != EntryKind::Directory
            {
                continue;
            }
            let destination = safe_join(&self.root, path)?;
            ensure_safe_parent(&self.root, &destination)?;
            if let Ok(metadata) = destination.symlink_metadata() {
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    remove_path(&destination)?;
                }
            }
            fs::create_dir_all(&destination)?;
            fs::set_permissions(&destination, fs::Permissions::from_mode(flat.entry.mode))?;
        }

        for (path, flat) in target {
            if !changed.contains(path)
                || !selected(path, selected_paths)
                || flat.entry.kind == EntryKind::Directory
            {
                continue;
            }
            let destination = safe_join(&self.root, path)?;
            ensure_safe_parent(&self.root, &destination)?;
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let must_remove = destination
                .symlink_metadata()
                .map(|metadata| {
                    metadata.is_dir()
                        || flat.entry.kind != EntryKind::File
                        || metadata.file_type().is_symlink()
                })
                .unwrap_or(false);
            if must_remove {
                remove_path(&destination)?;
            }
            match flat.entry.kind {
                EntryKind::File => self.restore_file(&destination, &flat.entry)?,
                EntryKind::Symlink => {
                    std::os::unix::fs::symlink(
                        OsString::from_vec(flat.entry.link_target.clone()),
                        &destination,
                    )?;
                }
                EntryKind::Fifo => {
                    let cpath = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
                    let result =
                        unsafe { libc::mkfifo(cpath.as_ptr(), flat.entry.mode as libc::mode_t) };
                    if result != 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                }
                EntryKind::SocketMarker => {}
                EntryKind::Directory => unreachable!(),
            }
            applied_operations += 1;
            if applied_operations == 1
                && std::env::var_os("AGIT_FAILPOINT").as_deref()
                    == Some(OsStr::new("rewind_after_first_change"))
            {
                std::process::exit(86);
            }
        }

        // Remove paths absent from the target, deepest first.
        let mut removals: Vec<_> = plan
            .changes
            .iter()
            .filter(|change| change.action == "remove")
            .map(|change| change.raw_path.clone())
            .collect();
        removals.sort_by_key(|path| std::cmp::Reverse(path.len()));
        for path in removals {
            if selected(&path, selected_paths) {
                let destination = safe_join(&self.root, &path)?;
                ensure_safe_parent(&self.root, &destination)?;
                if destination.symlink_metadata().is_ok() {
                    remove_path(&destination)?;
                }
            }
        }

        // Apply directory mtimes after child operations so materialization does
        // not overwrite the captured timestamp.
        for (path, flat) in target.iter().rev() {
            if !changed.contains(path)
                || !selected(path, selected_paths)
                || flat.entry.kind != EntryKind::Directory
            {
                continue;
            }
            let destination = safe_join(&self.root, path)?;
            let mtime = FileTime::from_unix_time(flat.entry.mtime_secs, flat.entry.mtime_nanos);
            filetime::set_file_mtime(destination, mtime)?;
        }
        Ok(())
    }

    fn restore_file(&self, destination: &Path, entry: &TreeEntry) -> anyhow::Result<()> {
        let blob: Blob = self.store.read_struct(
            &entry.target.context("file missing blob ID")?,
            ObjectKind::Blob,
        )?;
        let parent = destination.parent().context("file has no parent")?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        for chunk in blob.chunks {
            let bytes = self.store.read_bytes(&chunk.id, ObjectKind::Chunk)?;
            anyhow::ensure!(bytes.len() == chunk.len as usize, "chunk length mismatch");
            temp.write_all(&bytes)?;
        }
        temp.as_file().sync_all()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(entry.mode))?;
        if let Some(xattrs_id) = entry.xattrs {
            let xattrs: Xattrs = self.store.read_struct(&xattrs_id, ObjectKind::Xattrs)?;
            for xattr in xattrs.entries {
                xattr::set(temp.path(), OsStr::from_bytes(&xattr.name), &xattr.value)?;
            }
        }
        temp.persist(destination).map_err(|error| error.error)?;
        let mtime = FileTime::from_unix_time(entry.mtime_secs, entry.mtime_nanos);
        filetime::set_file_mtime(destination, mtime)?;
        Ok(())
    }

    fn acquire_mutation_lock(&self) -> anyhow::Result<File> {
        let path = self
            .store
            .workspace_data_dir(&self.workspace_id)
            .join("mutation.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.lock_exclusive()?;
        Ok(file)
    }

    fn restore_intent_path(&self) -> PathBuf {
        self.store
            .workspace_data_dir(&self.workspace_id)
            .join("restore.intent")
    }

    fn write_restore_intent(&self, intent: &RestoreIntent) -> anyhow::Result<()> {
        let path = self.restore_intent_path();
        atomic_write(&path, &serde_json::to_vec(intent)?)?;
        File::open(path.parent().context("restore intent has no parent")?)?.sync_all()?;
        Ok(())
    }

    fn clear_restore_intent(&self) -> anyhow::Result<()> {
        let path = self.restore_intent_path();
        if path.exists() {
            fs::remove_file(&path)?;
            File::open(path.parent().context("restore intent has no parent")?)?.sync_all()?;
        }
        Ok(())
    }

    fn recover_interrupted_rewind(&self) -> anyhow::Result<()> {
        let path = self.restore_intent_path();
        if !path.exists() {
            return Ok(());
        }
        let lock = self.acquire_mutation_lock()?;
        let intent: RestoreIntent = serde_json::from_slice(&fs::read(&path)?)?;
        eprintln!(
            "agit: recovering interrupted rewind; restoring pre-rewind snapshot {}",
            &id_hex(&intent.pre_snapshot)[..12]
        );
        let snapshot: Snapshot = self
            .store
            .read_struct(&intent.pre_snapshot, ObjectKind::Snapshot)?;
        let entries = self.flatten_tree(&snapshot.root_tree)?;
        let paths: Vec<PathBuf> = intent
            .paths
            .into_iter()
            .map(|path| PathBuf::from(OsString::from_vec(path)))
            .collect();
        let plan = self.plan_rewind(&intent.pre_snapshot, &paths)?;
        self.apply_rewind(&entries, &plan, &paths)?;
        self.clear_restore_intent()?;
        FileExt::unlock(&lock)?;
        Ok(())
    }
}

fn data_root() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("AGIT_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    let dirs = ProjectDirs::from("dev", "agit", "agit")
        .context("cannot determine application data directory")?;
    Ok(dirs.data_dir().to_owned())
}

fn new_workspace_id(root: &Path) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(root.as_os_str().as_bytes());
    let (secs, nanos) = now();
    hasher.update(&secs.to_le_bytes());
    hasher.update(&nanos.to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    hex::encode(&hasher.finalize().as_bytes()[..16])
}

fn now() -> (i64, u32) {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (duration.as_secs() as i64, duration.subsec_nanos())
}

fn metadata_time(metadata: &fs::Metadata) -> (i64, u32) {
    (metadata.mtime(), metadata.mtime_nsec().max(0) as u32)
}

fn stable_metadata(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
        && before.mode() == after.mode()
}

fn cached_matches(cached: &CachedFile, metadata: &fs::Metadata) -> bool {
    cached.device == metadata.dev()
        && cached.inode == metadata.ino()
        && cached.size == metadata.len()
        && cached.mtime_secs == metadata.mtime()
        && cached.mtime_nanos == metadata.mtime_nsec()
        && cached.ctime_secs == metadata.ctime()
        && cached.ctime_nanos == metadata.ctime_nsec()
        && cached.mode == metadata.mode()
}

fn cached_from_metadata(metadata: &fs::Metadata, blob_id: ObjectId) -> CachedFile {
    CachedFile {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        mtime_secs: metadata.mtime(),
        mtime_nanos: metadata.mtime_nsec(),
        ctime_secs: metadata.ctime(),
        ctime_nanos: metadata.ctime_nsec(),
        mode: metadata.mode(),
        blob_id,
    }
}

fn special_entry(name: Vec<u8>, kind: EntryKind, mode: u32, secs: i64, nanos: u32) -> TreeEntry {
    TreeEntry {
        name,
        kind,
        target: None,
        link_target: Vec::new(),
        mode,
        size: 0,
        mtime_secs: secs,
        mtime_nanos: nanos,
        xattrs: None,
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().context("path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn validate_name(name: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(!name.is_empty(), "empty path component in snapshot");
    anyhow::ensure!(
        name != b"." && name != b"..",
        "unsafe path component in snapshot"
    );
    anyhow::ensure!(
        !name.contains(&0) && !name.contains(&b'/'),
        "invalid path component in snapshot"
    );
    Ok(())
}

fn safe_join(root: &Path, relative: &[u8]) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(OsString::from_vec(relative.to_vec()));
    for component in path.components() {
        anyhow::ensure!(
            matches!(component, Component::Normal(_)),
            "unsafe rewind path"
        );
    }
    Ok(root.join(path))
}

fn ensure_safe_parent(root: &Path, destination: &Path) -> anyhow::Result<()> {
    let relative = destination
        .strip_prefix(root)
        .context("rewind path escaped workspace")?;
    let mut current = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let Component::Normal(name) = component else {
                bail!("unsafe rewind path")
            };
            current.push(name);
            if let Ok(metadata) = fs::symlink_metadata(&current) {
                anyhow::ensure!(
                    !metadata.file_type().is_symlink(),
                    "refusing to traverse symlink parent during rewind: {}",
                    current.display()
                );
            }
        }
    }
    Ok(())
}

fn selected(path: &[u8], selections: &[PathBuf]) -> bool {
    if selections.is_empty() {
        return true;
    }
    let candidate = Path::new(OsStr::from_bytes(path));
    selections
        .iter()
        .any(|selection| candidate == selection || candidate.starts_with(selection))
}

fn display_relative(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

fn remove_path(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn collect_sqlite_candidates(root: &Path, output: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for child in fs::read_dir(root)? {
        let child = child?;
        let path = child.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_sqlite_candidates(&path, output)?;
        } else if metadata.is_file() && sqlite_adapter::is_sqlite(&path) {
            output.push(path);
        }
    }
    Ok(())
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}
