//! A persisted read scope binds a guard's screening to one capture.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturePlan {
    pub token: String,
    pub full: bool,
    /// Physical paths include the composed directory instead of its logical alias.
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    size: u64,
    mode: u32,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    fn of(m: &fs::Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
            size: m.len(),
            mode: m.mode(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PreparedPlan {
    root: PathBuf,
    workspace: String,
    overlay: Option<PathBuf>,
    generation: String,
    head: Option<ObjectId>,
    policy: Vec<u8>,
    changed: Option<Vec<PathBuf>>,
    full: bool,
    candidates: BTreeMap<PathBuf, Option<Stamp>>,
}

impl FurrowRepository {
    /// Plans enumerate names and metadata, never file contents to be captured.
    pub fn capture_plan(&self, changed: Option<&[PathBuf]>) -> anyhow::Result<CapturePlan> {
        let policy = self.load_policy()?;
        let generation = self.store_generation()?;
        let full = changed.is_none()
            || changed
                .is_some_and(|paths| paths.iter().any(|p| capture_rules_changed(&self.root, p)))
            || !self.index_matches_head(&*self.open_path_index()?, &generation, &policy)?;
        let mut prepared = PreparedPlan {
            root: self.root.clone(),
            workspace: self.workspace_id.clone(),
            overlay: self.overlay.as_ref().map(|o| o.source().to_owned()),
            generation,
            head: self.store.workspace_head(&self.workspace_id)?,
            policy: self.plan_policy_identity()?,
            changed: if full {
                None
            } else {
                changed.map(|p| p.to_vec())
            },
            full,
            candidates: BTreeMap::new(),
        };
        if full {
            self.plan_directory(&self.root, &policy, &mut prepared)?;
        } else {
            for path in self.with_overlay_path(changed.unwrap_or_default()) {
                let absolute = if path.is_absolute() {
                    path
                } else {
                    self.root.join(path)
                };
                if let Some(relative) = self.logical_path(&absolute) {
                    if relative.is_empty() {
                        return self.capture_plan(None);
                    }
                    validate_relative_path(&relative)?;
                    if !is_identity_path(&relative) && !policy.excludes_bytes(&relative) {
                        self.plan_path(&self.physical_path(&relative)?, &policy, &mut prepared)?;
                    }
                }
            }
        }
        // Consistent database backups can also read sidecars even when only another file changed.
        for database in self.sqlite_candidates(&policy)? {
            self.plan_path(&database, &policy, &mut prepared)?;
            for suffix in ["-wal", "-shm", "-journal"] {
                let mut name = database.as_os_str().to_owned();
                name.push(suffix);
                self.plan_path(&PathBuf::from(name), &policy, &mut prepared)?;
            }
        }
        let bytes = serde_json::to_vec(&prepared)?;
        let token = blake3::hash(&bytes).to_hex().to_string();
        let directory = self.workspace_data_dir().join("capture-plans");
        fs::create_dir_all(&directory)?;
        // Replacing a complete file is sufficient: plans are disposable and never authoritative state.
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
        temporary.write_all(&bytes)?;
        crate::platform::rename(temporary.path(), directory.join(format!("{token}.json")))?;
        Ok(CapturePlan {
            token,
            full,
            paths: prepared.candidates.into_keys().collect(),
        })
    }

    pub fn seal_plan(
        &mut self,
        label: Option<String>,
        trigger: SnapshotTrigger,
        token: &str,
    ) -> anyhow::Result<ObjectId> {
        anyhow::ensure!(
            token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid capture plan token"
        );
        let path = self
            .workspace_data_dir()
            .join("capture-plans")
            .join(format!("{token}.json"));
        let bytes = fs::read(&path).context("capture plan is absent; re-plan")?;
        anyhow::ensure!(
            blake3::hash(&bytes).to_hex().as_str() == token,
            "capture plan token mismatch; re-plan"
        );
        let plan: PreparedPlan = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            plan.root == self.root
                && plan.workspace == self.workspace_id
                && plan.overlay == self.overlay.as_ref().map(|o| o.source().to_owned())
                && plan.generation == self.store_generation()?
                && plan.head == self.store.workspace_head(&self.workspace_id)?
                && plan.policy == self.plan_policy_identity()?,
            "capture plan is stale; re-plan"
        );
        let changed = plan.changed.clone();
        *self.capture_scope.borrow_mut() = Some(plan);
        let result = self.seal(label, trigger, changed.as_deref());
        self.capture_scope.borrow_mut().take();
        let _ = fs::remove_file(path);
        result
    }

    fn plan_policy_identity(&self) -> anyhow::Result<Vec<u8>> {
        let own = fs::read(self.root.join(".furrowpolicy")).unwrap_or_default();
        let composed = self
            .overlay
            .as_ref()
            .map(|o| fs::read(o.source().join(".furrowpolicy")).unwrap_or_default());
        Ok(blake3::hash(&serde_json::to_vec(&(own, composed))?)
            .as_bytes()
            .to_vec())
    }

    fn plan_path(
        &self,
        path: &Path,
        policy: &CapturePolicy,
        plan: &mut PreparedPlan,
    ) -> anyhow::Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let directory = metadata.is_dir() && !metadata.file_type().is_symlink();
        plan.candidates
            .insert(path.to_owned(), (!directory).then(|| Stamp::of(&metadata)));
        if directory {
            self.plan_directory(path, policy, plan)?;
        }
        Ok(())
    }

    fn plan_directory(
        &self,
        path: &Path,
        policy: &CapturePolicy,
        plan: &mut PreparedPlan,
    ) -> anyhow::Result<()> {
        let relative = self
            .logical_path(path)
            .context("plan directory outside tree")?;
        let children: Box<dyn Iterator<Item = std::io::Result<OsString>>> =
            match policy.tracked_children(&relative) {
                Some(names) => Box::new(names.into_iter().map(Ok)),
                None => self.children(path)?,
            };
        for name in children {
            let child = self.child_path(path, &name?);
            if child.starts_with(self.store.root()) || child == self.root.join(".furrow") {
                continue;
            }
            let relative = self
                .logical_path(&child)
                .context("plan path outside tree")?;
            if is_identity_path(&relative) || policy.excludes_bytes(&relative) {
                continue;
            }
            self.plan_path(&child, policy, plan)?;
        }
        Ok(())
    }

    /// SQLite opens journal names internally, so scoped backups read an isolated copy of screened bytes.
    pub(super) fn stage_scoped_database(
        &self,
        source: &Path,
        temp_dir: &Path,
    ) -> anyhow::Result<tempfile::TempDir> {
        fs::create_dir_all(temp_dir)?;
        let staged = tempfile::tempdir_in(temp_dir)?;
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut name = source.as_os_str().to_owned();
            name.push(suffix);
            let path = PathBuf::from(name);
            if !path.try_exists()? {
                continue;
            }
            let blob = self.capture_file(&path)?;
            let blob: Blob = self.store.read_struct(&blob, ObjectKind::Blob)?;
            let mut output = File::create(staged.path().join(format!("db{suffix}")))?;
            for chunk in blob.chunks {
                output.write_all(&self.store.read_bytes(&chunk.id, ObjectKind::Chunk)?)?;
            }
        }
        Ok(staged)
    }

    pub(super) fn check_capture_path(
        &self,
        path: &Path,
        metadata: &fs::Metadata,
    ) -> anyhow::Result<()> {
        if path.starts_with(self.store.root()) {
            return Ok(());
        }
        let scope = self.capture_scope.borrow();
        let Some(plan) = scope.as_ref() else {
            return Ok(());
        };
        match plan.candidates.get(path) {
            Some(None) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
            Some(Some(stamp)) if *stamp == Stamp::of(metadata) => Ok(()),
            _ => bail!("capture plan no longer covers {}; re-plan", path.display()),
        }
    }
}
