//! A watcher is advisory until an ordered event fence proves its queue has caught up.

use super::*;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

const QUEUE: usize = 8192;

fn read_access(event: &Event) -> bool {
    matches!(event.kind, EventKind::Access(_))
        && !matches!(
            event.kind,
            EventKind::Access(notify::event::AccessKind::Close(
                notify::event::AccessMode::Write
            ))
        )
}

pub(super) struct ChangeTracker {
    watcher: RecommendedWatcher,
    events: mpsc::Receiver<notify::Result<Event>>,
    overflow: Arc<AtomicBool>,
    fence: tempfile::TempDir,
    dirty: BTreeMap<PathBuf, u64>,
    sequence: u64,
    uncertain: bool,
    baseline: Option<ObjectId>,
    policy: Vec<u8>,
    generation: String,
    mounts: Vec<u8>,
}

impl ChangeTracker {
    fn new(repo: &FurrowRepository) -> anyhow::Result<Self> {
        let (sender, events) = mpsc::sync_channel(QUEUE);
        let overflow = Arc::new(AtomicBool::new(false));
        let flag = overflow.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            // Content reads generate access events; discarding them here prevents our own captures filling the queue.
            if event.as_ref().is_ok_and(read_access) {
                return;
            }
            if sender.try_send(event).is_err() {
                flag.store(true, Ordering::Release);
            }
        })?;
        fs::create_dir_all(repo.store.root().join("tmp"))?;
        let fence = tempfile::tempdir_in(repo.store.root().join("tmp"))?;
        let mut result = Self {
            watcher,
            events,
            overflow,
            fence,
            dirty: BTreeMap::new(),
            sequence: 0,
            uncertain: false,
            baseline: None,
            policy: repo.plan_policy_identity()?,
            generation: repo.store_generation()?,
            mounts: mount_identity()?,
        };
        // Register each directory before listing its children. Any setup gap is covered by the first full capture.
        let policy = repo.load_policy()?;
        result.watch_directory(repo, repo.root(), &policy)?;
        result
            .watcher
            .watch(result.fence.path(), RecursiveMode::NonRecursive)?;
        Ok(result)
    }

    fn watch_directory(
        &mut self,
        repo: &FurrowRepository,
        path: &Path,
        policy: &CapturePolicy,
    ) -> anyhow::Result<()> {
        check_filesystem(path)?;
        self.watcher.watch(path, RecursiveMode::NonRecursive)?;
        let relative = repo.logical_path(path).context("watch outside tree")?;
        let names: Box<dyn Iterator<Item = std::io::Result<OsString>>> =
            match policy.tracked_children(&relative) {
                Some(names) => Box::new(names.into_iter().map(Ok)),
                None => repo.children(path)?,
            };
        for name in names {
            let child = repo.child_path(path, &name?);
            let Some(relative) = repo.logical_path(&child) else {
                continue;
            };
            if child.starts_with(repo.store.root()) || is_identity_path(&relative) {
                continue;
            }
            // Linked nested repositories can keep their index/config outside every registered directory.
            anyhow::ensure!(
                !policy.uses_gitignore()
                    || path == repo.root()
                    || child.file_name().is_none_or(|name| name != ".git"),
                "nested Git scope requires a full walk"
            );
            if policy.uses_gitignore()
                && child.file_name().is_some_and(|name| name == ".gitignore")
                && fs::symlink_metadata(&child)?.file_type().is_symlink()
            {
                anyhow::bail!("linked ignore rules require a full walk");
            }
            if policy.excludes_bytes(&relative) {
                continue;
            }
            // Composed private state is visited on every request and needs no watcher.
            if repo.in_overlay(&relative) {
                continue;
            }
            let metadata = fs::symlink_metadata(&child)?;
            #[cfg(target_os = "linux")]
            if metadata.is_file() {
                // An inode watch also sees writes through hard-link aliases outside the folder.
                self.watcher.watch(&child, RecursiveMode::NonRecursive)?;
            }
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                self.watch_directory(repo, &child, policy)?;
            }
        }
        Ok(())
    }

    fn record(&mut self, event: notify::Result<Event>) {
        match event {
            Ok(event) => {
                if !event.need_rescan()
                    && !event.paths.is_empty()
                    && event.paths.iter().all(|p| p.starts_with(self.fence.path()))
                {
                    return;
                }
                if event.need_rescan() || matches!(event.kind, EventKind::Other | EventKind::Any) {
                    self.uncertain = true;
                }
                if read_access(&event) {
                    return;
                }
                // A regular create needs an inode watch before narrowing, including writes through outside aliases.
                // Directory topology and renames still invalidate registrations and widen the scope.
                let regular_create = matches!(
                    event.kind,
                    EventKind::Create(notify::event::CreateKind::File)
                ) && !event.paths.is_empty()
                    && cfg!(target_os = "linux")
                    && event.paths.iter().all(|path| {
                        fs::symlink_metadata(path)
                            .is_ok_and(|m| m.is_file() && !m.file_type().is_symlink())
                            && self
                                .watcher
                                .watch(path, RecursiveMode::NonRecursive)
                                .is_ok()
                    });
                if matches!(
                    event.kind,
                    EventKind::Modify(notify::event::ModifyKind::Name(_))
                        | EventKind::Create(_)
                        | EventKind::Remove(_)
                ) && !regular_create
                {
                    self.uncertain = true;
                }
                for path in event.paths {
                    if path.starts_with(self.fence.path()) {
                        continue;
                    }
                    self.sequence += 1;
                    self.dirty.insert(path, self.sequence);
                    if self.dirty.len() > QUEUE {
                        self.uncertain = true;
                        self.dirty.clear();
                    }
                }
            }
            Err(_) => self.uncertain = true,
        }
    }

    fn catch_up(&mut self) {
        // Linux delivers all watches on one ordered inotify queue. Other backends have no such fence;
        // until they expose an equivalent ordering guarantee their event hints always widen to a walk.
        if !cfg!(target_os = "linux") {
            while let Ok(event) = self.events.try_recv() {
                self.record(event);
            }
            if self.overflow.swap(false, Ordering::AcqRel) {
                self.uncertain = true;
            }
            return;
        }
        self.sequence += 1;
        let marker = self.fence.path().join(format!("{}", self.sequence));
        if fs::write(&marker, []).is_err() {
            self.uncertain = true;
            return;
        }
        let end = Instant::now() + Duration::from_millis(100);
        loop {
            match self
                .events
                .recv_timeout(end.saturating_duration_since(Instant::now()))
            {
                Ok(event) => {
                    let seen = event.as_ref().is_ok_and(|e| e.paths.contains(&marker));
                    self.record(event);
                    if seen {
                        break;
                    }
                }
                Err(_) => {
                    self.uncertain = true;
                    break;
                }
            }
        }
        let _ = fs::remove_file(marker);
        if self.overflow.swap(false, Ordering::AcqRel) {
            self.uncertain = true;
        }
    }
}

// Mount changes can hide a registered inode without producing a directory event.
fn mount_identity() -> anyhow::Result<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        Ok(fs::read("/proc/self/mountinfo")?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(Vec::new())
    }
}

fn check_filesystem(path: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // The kernel fills the complete statfs structure on success; unknown filesystems use the full walker.
        let result = unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let kind = unsafe { stat.assume_init() }.f_type as u64;
        anyhow::ensure!(
            matches!(
                kind,
                0xef53
                    | 0x58465342
                    | 0x9123683e
                    | 0x01021994
                    | 0x858458f6
                    | 0xf2f52010
                    | 0x794c7630
            ),
            "filesystem needs a full walk"
        );
    }
    let _ = path;
    Ok(())
}

impl FurrowRepository {
    /// Retained daemon repositories track changes; one-shot callers continue to use full walks.
    pub fn enable_change_tracking(&self) {
        self.store.retain_file_cache();
        *self.change_tracker.borrow_mut() = Some(ChangeTracker::new(self).ok());
    }

    // A replacement earns its baseline only from a full capture AFTER registration.
    pub(super) fn prepare_full_tracking(&self) -> bool {
        let rebuild = self
            .change_tracker
            .borrow()
            .as_ref()
            .is_some_and(|t| t.as_ref().is_some_and(|t| t.uncertain));
        if rebuild {
            self.enable_change_tracking();
        }
        rebuild
    }

    pub(super) fn select_tracked_changes(
        &self,
        changed: Option<&[PathBuf]>,
        tracked: Option<Vec<PathBuf>>,
    ) -> Option<Vec<PathBuf>> {
        // Explicit hints retain their exact scope. Unlisted watcher events stay pending for an unknown capture.
        changed.map(<[PathBuf]>::to_vec).or(tracked)
    }

    pub(super) fn tracked_changes(&self) -> anyhow::Result<(Option<Vec<PathBuf>>, u64)> {
        let mut tracker = self.change_tracker.borrow_mut();
        let Some(Some(tracker)) = tracker.as_mut() else {
            return Ok((None, 0));
        };
        tracker.catch_up();
        if tracker.policy != self.plan_policy_identity()?
            || tracker.generation != self.store_generation()?
            || tracker.mounts != mount_identity()?
        {
            tracker.uncertain = true;
        }
        if !cfg!(target_os = "linux")
            || tracker.uncertain
            || tracker.baseline != self.store.workspace_head(&self.workspace_id)?
        {
            return Ok((None, tracker.sequence));
        }
        let mut paths = Vec::new();
        for path in tracker.dirty.keys() {
            let Some(relative) = self.logical_path(path) else {
                continue;
            };
            if is_identity_path(&relative) || path.starts_with(self.store.root()) {
                continue;
            }
            if relative.is_empty() {
                return Ok((None, tracker.sequence));
            }
            if capture_rules_changed(&self.root, path) {
                tracker.uncertain = true;
                return Ok((None, tracker.sequence));
            }
            paths.push(path.clone());
        }
        Ok((Some(paths), tracker.sequence))
    }

    pub(super) fn finish_tracked_capture(
        &self,
        id: ObjectId,
        watermark: u64,
        captured: Option<&[PathBuf]>,
        trusted_baseline: bool,
    ) -> anyhow::Result<()> {
        let full = captured.is_none();
        let rebuild = self
            .change_tracker
            .borrow()
            .as_ref()
            .is_some_and(|tracker| full && tracker.as_ref().is_some_and(|t| t.uncertain));
        if rebuild {
            // Installing watches after a capture leaves a gap. The replacement must itself earn a full baseline.
            self.enable_change_tracking();
            return Ok(());
        }
        if let Some(Some(tracker)) = self.change_tracker.borrow_mut().as_mut() {
            if !full && (!trusted_baseline || tracker.uncertain) {
                return Ok(());
            }
            // A scoped capture cannot consume a later event outside the screened paths, even if
            // that event arrived before this operation's second queue fence.
            tracker.dirty.retain(|path, sequence| {
                *sequence > watermark
                    || captured.is_some_and(|paths| {
                        !paths.iter().any(|p| {
                            let p = if p.is_absolute() {
                                p.clone()
                            } else {
                                self.root.join(p)
                            };
                            path.starts_with(p)
                        })
                    })
            });
            tracker.baseline = Some(id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn q3_native_windows_backend_reports_directory_writes() {
        // Q3: the retained daemon's native ReadDirectoryChangesW registration delivers real file changes.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        fs::create_dir(&root).unwrap();
        let repo = FurrowRepository::attach_timeline_in(
            &temp.path().join("data"),
            &root,
            Some("native"),
            None,
        )
        .unwrap();
        let mut tracker = ChangeTracker::new(&repo).unwrap();
        let path = repo.root().join("reported");
        fs::write(&path, b"native notification").unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while !tracker.dirty.contains_key(&path) {
            let event = tracker
                .events
                .recv_timeout(end.saturating_duration_since(Instant::now()))
                .expect("native watcher must report the write");
            tracker.record(event);
        }
        assert!(tracker.dirty.contains_key(&path));
    }

    #[test]
    fn q3_overflow_death_and_install_failure_report_full_plans() {
        // Q3: overflow, a dead event source and an unavailable watcher each widen the advertised read scope.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a"), b"first").unwrap();
        let mut repo = FurrowRepository::attach_timeline_in(
            &temp.path().join("data"),
            &root,
            Some("watch"),
            None,
        )
        .unwrap();
        repo.enable_change_tracking();
        repo.seal(None, SnapshotTrigger::AgentRun, None).unwrap();
        if let Some(Some(tracker)) = repo.change_tracker.borrow_mut().as_mut() {
            tracker.overflow.store(true, Ordering::Release);
        }
        assert!(repo.capture_plan(None).unwrap().full);
        repo.enable_change_tracking();
        repo.seal(None, SnapshotTrigger::AgentRun, None).unwrap();
        if let Some(Some(tracker)) = repo.change_tracker.borrow_mut().as_mut() {
            let (sender, receiver) = mpsc::channel();
            drop(sender);
            tracker.events = receiver;
        }
        assert!(repo.capture_plan(None).unwrap().full);
        *repo.change_tracker.borrow_mut() = Some(None);
        assert!(repo.capture_plan(None).unwrap().full);
    }

    #[test]
    fn q3_ignored_directories_have_no_watches_or_dirty_paths() {
        // Q3/G1: an excluded dependency tree is never enumerated or registered by change tracking.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        fs::create_dir(&root).unwrap();
        fs::write(root.join(".furrowpolicy"), b"exclude ignored\n").unwrap();
        fs::create_dir(root.join("ignored")).unwrap();
        fs::write(root.join("ignored/a"), b"first").unwrap();
        let mut repo = FurrowRepository::attach_timeline_in(
            &temp.path().join("data"),
            &root,
            Some("watch"),
            None,
        )
        .unwrap();
        repo.enable_change_tracking();
        repo.seal(None, SnapshotTrigger::AgentRun, None).unwrap();
        fs::write(root.join("ignored/a"), b"second").unwrap();
        let plan = repo.capture_plan(None).unwrap();
        assert!(!plan
            .paths
            .iter()
            .any(|p| p.starts_with(repo.root().join("ignored"))));
        if cfg!(target_os = "linux") {
            assert!(!plan.full);
        }
    }
}
