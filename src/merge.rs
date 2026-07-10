//! Deterministic three-way planning for complete workspace trees.

use crate::model::{EntryKind, TreeEntry};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, Serialize)]
pub struct MergePlan {
    pub changes: Vec<MergeChange>,
    pub conflicts: Vec<MergeConflict>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeChange {
    #[serde(with = "serde_bytes")]
    pub path: Vec<u8>,
    pub action: MergeAction,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "entry")]
pub enum MergeAction {
    Set(TreeEntry),
    Remove,
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeConflict {
    #[serde(with = "serde_bytes")]
    pub path: Vec<u8>,
    pub kind: ConflictKind,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    ModifyModify,
    DeleteModify,
    Type,
    Ancestor,
}

impl fmt::Display for ConflictKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ModifyModify => "modify/modify",
            Self::DeleteModify => "delete/modify",
            Self::Type => "type",
            Self::Ancestor => "ancestor",
        })
    }
}

pub fn plan(
    base: &BTreeMap<Vec<u8>, TreeEntry>,
    ours: &BTreeMap<Vec<u8>, TreeEntry>,
    theirs: &BTreeMap<Vec<u8>, TreeEntry>,
) -> MergePlan {
    let mut paths = BTreeSet::new();
    paths.extend(base.keys().cloned());
    paths.extend(ours.keys().cloned());
    paths.extend(theirs.keys().cloned());

    let mut changes = Vec::new();
    let mut conflicts = Vec::new();
    for path in paths {
        let base_entry = base.get(&path);
        let ours_entry = ours.get(&path);
        let theirs_entry = theirs.get(&path);
        if equivalent(ours_entry, theirs_entry) {
            continue;
        }
        if equivalent(base_entry, ours_entry) {
            changes.push(MergeChange {
                path,
                action: theirs_entry
                    .cloned()
                    .map_or(MergeAction::Remove, MergeAction::Set),
            });
            continue;
        }
        if equivalent(base_entry, theirs_entry) {
            continue;
        }

        conflicts.push(MergeConflict {
            path,
            kind: direct_conflict(base_entry, ours_entry, theirs_entry),
        });
    }

    let mut ancestor_conflicts = BTreeSet::new();
    for (path, base_entry) in base {
        if base_entry.kind != EntryKind::Directory {
            continue;
        }
        let ours_is_directory = ours
            .get(path)
            .is_some_and(|entry| entry.kind == EntryKind::Directory);
        let theirs_is_directory = theirs
            .get(path)
            .is_some_and(|entry| entry.kind == EntryKind::Directory);
        if !ours_is_directory
            && theirs_is_directory
            && subtree_changed(base, theirs, path)
            && !equivalent(ours.get(path), theirs.get(path))
        {
            ancestor_conflicts.insert(path.clone());
        }
        if !theirs_is_directory
            && ours_is_directory
            && subtree_changed(base, ours, path)
            && !equivalent(ours.get(path), theirs.get(path))
        {
            ancestor_conflicts.insert(path.clone());
        }
    }
    for path in ancestor_conflicts {
        if !conflicts.iter().any(|conflict| conflict.path == path) {
            conflicts.push(MergeConflict {
                path: path.clone(),
                kind: ConflictKind::Ancestor,
            });
        }
    }
    conflicts.sort_by(|left, right| left.path.cmp(&right.path));
    conflicts.dedup_by(|left, right| left.path == right.path);
    changes.retain(|change| {
        !conflicts.iter().any(|conflict| {
            change.path == conflict.path || is_descendant(&change.path, &conflict.path)
        })
    });
    MergePlan { changes, conflicts }
}

fn equivalent(left: Option<&TreeEntry>, right: Option<&TreeEntry>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            let mut left = left.clone();
            let mut right = right.clone();
            left.mtime_secs = 0;
            left.mtime_nanos = 0;
            right.mtime_secs = 0;
            right.mtime_nanos = 0;
            if left.kind == EntryKind::Directory && right.kind == EntryKind::Directory {
                left.target = None;
                right.target = None;
            }
            left == right
        }
        _ => false,
    }
}

fn direct_conflict(
    base: Option<&TreeEntry>,
    ours: Option<&TreeEntry>,
    theirs: Option<&TreeEntry>,
) -> ConflictKind {
    if ours.is_none() || theirs.is_none() {
        return ConflictKind::DeleteModify;
    }
    let ours = ours.expect("checked above");
    let theirs = theirs.expect("checked above");
    if ours.kind != theirs.kind || base.is_some_and(|entry| entry.kind != ours.kind) {
        ConflictKind::Type
    } else {
        ConflictKind::ModifyModify
    }
}

fn subtree_changed(
    base: &BTreeMap<Vec<u8>, TreeEntry>,
    side: &BTreeMap<Vec<u8>, TreeEntry>,
    directory: &[u8],
) -> bool {
    base.iter()
        .filter(|(path, _)| is_descendant(path, directory))
        .any(|(path, entry)| !equivalent(Some(entry), side.get(path)))
        || side
            .iter()
            .filter(|(path, _)| is_descendant(path, directory))
            .any(|(path, entry)| !equivalent(base.get(path), Some(entry)))
}

fn is_descendant(path: &[u8], ancestor: &[u8]) -> bool {
    path.len() > ancestor.len()
        && path.starts_with(ancestor)
        && path.get(ancestor.len()) == Some(&b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(byte: u8) -> TreeEntry {
        TreeEntry {
            name: vec![byte],
            kind: EntryKind::File,
            target: Some([byte; 32]),
            link_target: Vec::new(),
            mode: 0o100644,
            size: 1,
            mtime_secs: 0,
            mtime_nanos: 0,
            xattrs: None,
        }
    }

    fn directory(target: u8) -> TreeEntry {
        TreeEntry {
            name: b"dir".to_vec(),
            kind: EntryKind::Directory,
            target: Some([target; 32]),
            link_target: Vec::new(),
            mode: 0o40755,
            size: 0,
            mtime_secs: 0,
            mtime_nanos: 0,
            xattrs: None,
        }
    }

    #[test]
    fn merges_independent_changes_and_ignores_directory_merkle_ids() {
        let base = BTreeMap::from([
            (b"dir".to_vec(), directory(1)),
            (b"dir/a".to_vec(), file(1)),
            (b"dir/b".to_vec(), file(2)),
        ]);
        let mut ours = base.clone();
        ours.insert(b"dir".to_vec(), directory(2));
        ours.insert(b"dir/a".to_vec(), file(3));
        let mut theirs = base.clone();
        theirs.insert(b"dir".to_vec(), directory(3));
        theirs.insert(b"dir/b".to_vec(), file(4));

        let plan = plan(&base, &ours, &theirs);
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].path, b"dir/b");
    }

    #[test]
    fn reports_divergent_and_delete_modify_conflicts() {
        let base = BTreeMap::from([(b"value".to_vec(), file(1))]);
        let ours = BTreeMap::from([(b"value".to_vec(), file(2))]);
        let theirs = BTreeMap::from([(b"value".to_vec(), file(3))]);
        assert_eq!(
            plan(&base, &ours, &theirs).conflicts[0].kind,
            ConflictKind::ModifyModify
        );
        assert_eq!(
            plan(&base, &BTreeMap::new(), &theirs).conflicts[0].kind,
            ConflictKind::DeleteModify
        );
    }

    #[test]
    fn identical_additions_with_different_timestamps_are_already_converged() {
        let base = BTreeMap::new();
        let ours = BTreeMap::from([(b"coord/task".to_vec(), file(1))]);
        let mut timestamped = file(1);
        timestamped.mtime_secs = 42;
        timestamped.mtime_nanos = 7;
        let theirs = BTreeMap::from([(b"coord/task".to_vec(), timestamped)]);

        let plan = plan(&base, &ours, &theirs);
        assert!(plan.conflicts.is_empty());
        assert!(plan.changes.is_empty());
    }

    #[test]
    fn directory_delete_conflicts_with_new_descendant() {
        let base = BTreeMap::from([
            (b"dir".to_vec(), directory(1)),
            (b"dir/old".to_vec(), file(1)),
        ]);
        let ours = BTreeMap::new();
        let mut theirs = base.clone();
        theirs.insert(b"dir/new".to_vec(), file(2));
        theirs.insert(b"dir".to_vec(), directory(2));

        let plan = plan(&base, &ours, &theirs);
        assert!(plan
            .conflicts
            .iter()
            .any(|conflict| conflict.path == b"dir"));
        assert!(plan.changes.is_empty());
    }
}
