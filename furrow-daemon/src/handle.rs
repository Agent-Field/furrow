//! A retained repository and its pending group commit, owned by one writer thread.

use crate::wire::Target;
use furrow::model::{ObjectId, SnapshotTrigger};
use furrow::{store::ObjectStore, FurrowRepository};
use std::path::PathBuf;

pub struct Handle {
    target: Target,
    repo: Option<FurrowRepository>,
    store: Option<ObjectStore>,
    dirty: bool,
}

impl Handle {
    pub fn new(target: Target) -> Self {
        Self {
            target,
            repo: None,
            store: None,
            dirty: false,
        }
    }
    pub fn sharing(target: Target, other: &mut Self) -> anyhow::Result<Self> {
        Ok(Self {
            target,
            repo: None,
            store: Some(other.store()?.clone()),
            dirty: false,
        })
    }

    pub fn store(&mut self) -> anyhow::Result<&ObjectStore> {
        if self.store.is_none() {
            self.store = Some(ObjectStore::open(furrow::repository::store_path(
                &self.target.data_dir,
            ))?);
        }
        let store = self.store.as_ref().expect("opened above");
        store.retain_file_cache();
        Ok(store)
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    pub fn release(&mut self) -> anyhow::Result<()> {
        self.sync()?;
        self.repo = None;
        Ok(())
    }

    pub fn repo(&mut self) -> anyhow::Result<&mut FurrowRepository> {
        if self.repo.is_none() {
            let store = self.store()?.clone();
            let target = &self.target;
            self.repo = Some(FurrowRepository::open_timeline_store(
                store,
                &target.tree,
                target.cell_dir.clone(),
                target.timeline.as_deref(),
            )?);
            self.repo
                .as_ref()
                .expect("opened above")
                .enable_change_tracking();
        }
        Ok(self.repo.as_mut().expect("opened above"))
    }

    pub fn attach(&mut self) -> anyhow::Result<&mut FurrowRepository> {
        if self.repo.is_none() {
            let store = self.store()?.clone();
            let t = &self.target;
            self.repo = Some(FurrowRepository::attach_timeline_store(
                store,
                &t.tree,
                t.timeline.as_deref(),
                t.cell_dir.clone(),
            )?);
            self.repo
                .as_ref()
                .expect("attached above")
                .enable_change_tracking();
        }
        Ok(self.repo.as_mut().expect("attached above"))
    }

    pub fn seal(
        &mut self,
        label: String,
        changed: Option<Vec<PathBuf>>,
        plan: Option<String>,
        durable: bool,
    ) -> anyhow::Result<ObjectId> {
        let repo = self.attach()?;
        repo.defer_durability()?;
        // A failed capture may have written orphan objects; flush those with the next group too.
        self.dirty = true;
        let repo = self.repo.as_mut().expect("attached above");
        let result = match plan {
            Some(token) => repo.seal_plan(Some(label), SnapshotTrigger::AgentRun, &token),
            None => repo.seal(Some(label), SnapshotTrigger::AgentRun, changed.as_deref()),
        };
        repo.resume_durability();
        let id = result?;
        if durable {
            self.sync()?;
        }
        Ok(id)
    }

    pub fn sync(&mut self) -> anyhow::Result<()> {
        if let Some(repo) = &self.repo {
            repo.sync()?;
        } else if let Some(store) = &self.store {
            store.sync()?;
        }
        self.dirty = false;
        Ok(())
    }

    pub fn checkpoint_capture_index(&self) -> anyhow::Result<()> {
        if let Some(repo) = &self.repo {
            repo.checkpoint_capture_index()?;
        }
        Ok(())
    }

    pub fn pending(&self) -> bool {
        self.dirty
    }
    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    pub fn flush_pending(&mut self) -> anyhow::Result<()> {
        if self.dirty {
            self.sync()?;
        }
        Ok(())
    }
}
