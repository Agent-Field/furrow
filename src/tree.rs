//! Bounded Merkle pages for directories, including very large flat trees.

use crate::model::{ObjectId, ObjectKind, Tree, TreeEntry, TreePage};
use crate::store::ObjectStore;
use anyhow::Context;

/// Leaves and branches stay comfortably below the pack object's global limit.
pub const MAX_TREE_PAGE_BYTES: usize = 64 * 1024;
const LEAF_OVERHEAD: usize = b"{\"entries\":[]}".len();
const BRANCH_OVERHEAD: usize = b"{\"pages\":[]}".len();

pub fn write(store: &ObjectStore, entries: Vec<TreeEntry>) -> anyhow::Result<ObjectId> {
    let mut builder = Builder::new(store);
    for entry in entries {
        builder.push(entry)?;
    }
    builder.finish()
}

pub struct Builder<'a> {
    store: &'a ObjectStore,
    page: Vec<TreeEntry>,
    page_bytes: usize,
    leaves: Vec<TreePage>,
}

impl<'a> Builder<'a> {
    pub fn new(store: &'a ObjectStore) -> Self {
        Self {
            store,
            page: Vec::new(),
            page_bytes: LEAF_OVERHEAD,
            leaves: Vec::new(),
        }
    }

    pub fn push(&mut self, entry: TreeEntry) -> anyhow::Result<()> {
        if let Some(previous) = self.page.last() {
            anyhow::ensure!(
                previous.name < entry.name,
                "directory entries must be strictly sorted and unique"
            );
        } else if let Some(previous) = self.leaves.last() {
            anyhow::ensure!(
                previous.last_name < entry.name,
                "directory entries must be strictly sorted and unique"
            );
        }
        let item_bytes = serde_json::to_vec(&entry)?.len();
        let separator = usize::from(!self.page.is_empty());
        if self.page_bytes + separator + item_bytes > MAX_TREE_PAGE_BYTES && !self.page.is_empty() {
            self.flush_leaf()?;
        }
        let separator = usize::from(!self.page.is_empty());
        anyhow::ensure!(
            self.page_bytes + separator + item_bytes <= MAX_TREE_PAGE_BYTES,
            "single directory entry exceeds 64 KiB"
        );
        self.page_bytes += separator + item_bytes;
        self.page.push(entry);
        Ok(())
    }

    pub fn finish(mut self) -> anyhow::Result<ObjectId> {
        if !self.page.is_empty() {
            self.flush_leaf()?;
        }
        if self.leaves.is_empty() {
            return self.store.put_struct(
                ObjectKind::Tree,
                &Tree {
                    entries: Vec::new(),
                    pages: Vec::new(),
                },
            );
        }
        if self.leaves.len() == 1 {
            return Ok(self.leaves[0].target);
        }
        let mut level = self.leaves;
        while level.len() > 1 {
            level = write_branch_level(self.store, level)?;
        }
        Ok(level[0].target)
    }

    fn flush_leaf(&mut self) -> anyhow::Result<()> {
        let entries = std::mem::take(&mut self.page);
        self.leaves.push(store_leaf(self.store, entries)?);
        self.page_bytes = LEAF_OVERHEAD;
        Ok(())
    }
}

/// Visits directory entries in bytewise name order while retaining at most one
/// decoded 64 KiB page per tree level.
pub fn for_each_entry<F>(store: &ObjectStore, root: &ObjectId, mut visitor: F) -> anyhow::Result<()>
where
    F: FnMut(TreeEntry) -> anyhow::Result<()>,
{
    let mut stack = vec![*root];
    while let Some(id) = stack.pop() {
        let tree: Tree = store.read_struct(&id, ObjectKind::Tree)?;
        anyhow::ensure!(
            tree.entries.is_empty() || tree.pages.is_empty(),
            "tree node mixes leaf entries and branch pages"
        );
        if tree.pages.is_empty() {
            for entry in tree.entries {
                visitor(entry)?;
            }
        } else {
            for page in tree.pages.into_iter().rev() {
                stack.push(page.target);
            }
        }
    }
    Ok(())
}

fn write_branch_level(store: &ObjectStore, pages: Vec<TreePage>) -> anyhow::Result<Vec<TreePage>> {
    let mut output = Vec::new();
    let mut branch = Vec::new();
    let mut encoded_bytes = BRANCH_OVERHEAD;
    for page in pages {
        let item_bytes = serde_json::to_vec(&page)?.len();
        let separator = usize::from(!branch.is_empty());
        if encoded_bytes + separator + item_bytes > MAX_TREE_PAGE_BYTES && !branch.is_empty() {
            output.push(store_branch(store, branch)?);
            branch = Vec::new();
            encoded_bytes = BRANCH_OVERHEAD;
        }
        let separator = usize::from(!branch.is_empty());
        anyhow::ensure!(
            encoded_bytes + separator + item_bytes <= MAX_TREE_PAGE_BYTES,
            "single tree page reference exceeds 64 KiB"
        );
        encoded_bytes += separator + item_bytes;
        branch.push(page);
    }
    if !branch.is_empty() {
        output.push(store_branch(store, branch)?);
    }
    Ok(output)
}

fn store_leaf(store: &ObjectStore, entries: Vec<TreeEntry>) -> anyhow::Result<TreePage> {
    let first_name = entries.first().context("empty tree leaf")?.name.clone();
    let last_name = entries.last().context("empty tree leaf")?.name.clone();
    let entry_count = entries.len() as u64;
    let tree = Tree {
        entries,
        pages: Vec::new(),
    };
    anyhow::ensure!(
        serde_json::to_vec(&tree)?.len() <= MAX_TREE_PAGE_BYTES,
        "tree leaf exceeded page limit"
    );
    let target = store.put_struct(ObjectKind::Tree, &tree)?;
    Ok(TreePage {
        first_name,
        last_name,
        entry_count,
        target,
    })
}

fn store_branch(store: &ObjectStore, pages: Vec<TreePage>) -> anyhow::Result<TreePage> {
    let first_name = pages
        .first()
        .context("empty tree branch")?
        .first_name
        .clone();
    let last_name = pages.last().context("empty tree branch")?.last_name.clone();
    let entry_count = pages.iter().map(|page| page.entry_count).sum();
    let tree = Tree {
        entries: Vec::new(),
        pages,
    };
    anyhow::ensure!(
        serde_json::to_vec(&tree)?.len() <= MAX_TREE_PAGE_BYTES,
        "tree branch exceeded page limit"
    );
    let target = store.put_struct(ObjectKind::Tree, &tree)?;
    Ok(TreePage {
        first_name,
        last_name,
        entry_count,
        target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntryKind;

    #[test]
    fn large_flat_directory_is_paged_and_walks_in_order() {
        let temporary = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(temporary.path().join("store")).unwrap();
        let entries: Vec<_> = (0..20_000)
            .map(|index| TreeEntry {
                name: format!("file-{index:08}.txt").into_bytes(),
                kind: EntryKind::File,
                target: Some([index as u8; 32]),
                link_target: Vec::new(),
                mode: 0o100644,
                size: index,
                mtime_secs: 0,
                mtime_nanos: 0,
                xattrs: None,
            })
            .collect();

        let root = write(&store, entries).unwrap();
        let root_bytes = store.read_bytes(&root, ObjectKind::Tree).unwrap();
        assert!(root_bytes.len() <= MAX_TREE_PAGE_BYTES);
        let root_tree: Tree = serde_json::from_slice(&root_bytes).unwrap();
        assert!(!root_tree.pages.is_empty());

        let mut previous = Vec::new();
        let mut count = 0_u64;
        for_each_entry(&store, &root, |entry| {
            assert!(previous.is_empty() || previous < entry.name);
            previous = entry.name;
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 20_000);
    }
}
