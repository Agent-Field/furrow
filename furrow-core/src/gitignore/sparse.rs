//! Sparse index entries are resolved on demand through Git trees, never through directory listings.

use super::{bytes_path, valid_child_name};
use gix_index::hash::ObjectId;
use gix_object::FindExt;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
struct Node {
    id: ObjectId,
    tree: bool,
    gitlink: bool,
}

#[derive(Debug)]
struct Names {
    entries: BTreeMap<Vec<u8>, Node>,
    folded: BTreeMap<Vec<u8>, Vec<u8>>,
}

type TreeCache = HashMap<ObjectId, Arc<Names>>;

#[derive(Debug)]
pub(super) struct Sparse {
    roots: BTreeMap<Vec<u8>, ObjectId>,
    folded: BTreeMap<Vec<u8>, Vec<u8>>,
    objects: PathBuf,
    trees: Mutex<TreeCache>,
}

impl Sparse {
    pub(super) fn new(roots: BTreeMap<Vec<u8>, ObjectId>, objects: PathBuf) -> Self {
        let folded = roots
            .keys()
            .map(|p| (p.to_ascii_lowercase(), p.clone()))
            .collect();
        Self {
            roots,
            folded,
            objects,
            trees: Mutex::default(),
        }
    }

    fn tree(&self, id: &ObjectId) -> Option<Arc<Names>> {
        let mut cache = self.trees.lock().unwrap();
        if let Some(names) = cache.get(id) {
            return Some(names.clone());
        }
        // Git object IDs are immutable; only trees needed by materialized paths are decoded and cached.
        let names = (|| {
            let db = gix_odb::at(&self.objects).ok()?;
            let mut buffer = Vec::new();
            let object = db.find(id, &mut buffer).ok()?;
            if object.kind != gix_object::Kind::Tree {
                return None;
            }
            let mut entries = BTreeMap::new();
            for entry in gix_object::TreeRefIter::from_bytes(object.data) {
                let entry = entry.ok()?;
                if !valid_child_name(entry.filename) {
                    return None;
                }
                let node = Node {
                    id: entry.oid.to_owned(),
                    tree: entry.mode.is_tree(),
                    gitlink: entry.mode.is_commit(),
                };
                if entries.insert(entry.filename.to_vec(), node).is_some() {
                    return None;
                }
            }
            let folded = entries
                .keys()
                .map(|p| (p.to_ascii_lowercase(), p.clone()))
                .collect();
            Some(Arc::new(Names { entries, folded }))
        })()?;
        cache.insert(*id, names.clone());
        Some(names)
    }

    fn lookup(&self, path: &[u8], insensitive: bool) -> Option<Node> {
        let mut ancestor = path;
        loop {
            let key = if insensitive {
                self.folded
                    .get(&ancestor.to_ascii_lowercase())
                    .map(Vec::as_slice)
            } else {
                Some(ancestor)
            };
            if let Some(id) = key.and_then(|key| self.roots.get(key)) {
                let mut node = Node {
                    id: *id,
                    tree: true,
                    gitlink: false,
                };
                let suffix = path[ancestor.len()..]
                    .strip_prefix(b"/")
                    .unwrap_or_default();
                if !suffix.is_empty() {
                    for part in suffix.split(|b| *b == b'/') {
                        if !node.tree {
                            return None;
                        }
                        let names = self.tree(&node.id)?;
                        let key = if insensitive {
                            names.folded.get(&part.to_ascii_lowercase())?.as_slice()
                        } else {
                            part
                        };
                        node = names.entries.get(key)?.clone();
                    }
                }
                return Some(node);
            }
            ancestor = &ancestor[..ancestor.iter().rposition(|b| *b == b'/')?];
        }
    }

    pub(super) fn is_file(&self, path: &[u8], insensitive: bool) -> bool {
        self.lookup(path, insensitive)
            .is_some_and(|node| !node.tree)
    }

    pub(super) fn is_gitlink(&self, path: &[u8], insensitive: bool) -> bool {
        self.lookup(path, insensitive)
            .is_some_and(|node| node.gitlink)
    }

    pub(super) fn has_descendant(&self, prefix: &[u8], insensitive: bool) -> bool {
        let below = if insensitive {
            let folded = prefix.to_ascii_lowercase();
            self.folded
                .range(folded.clone()..)
                .next()
                .is_some_and(|(p, _)| p.starts_with(&folded))
        } else {
            self.roots
                .range(prefix.to_vec()..)
                .next()
                .is_some_and(|(p, _)| p.starts_with(prefix))
        };
        below
            || self
                .lookup(prefix.strip_suffix(b"/").unwrap_or(prefix), insensitive)
                .filter(|node| node.tree)
                .and_then(|node| self.tree(&node.id))
                .is_some_and(|names| !names.entries.is_empty())
    }

    pub(super) fn children(&self, prefix: &[u8], insensitive: bool) -> BTreeSet<OsString> {
        let name = |path: &[u8]| {
            bytes_path(path[prefix.len()..].split(|b| *b == b'/').next().unwrap()).into_os_string()
        };
        let mut result: BTreeSet<_> = if insensitive {
            let folded = prefix.to_ascii_lowercase();
            self.folded
                .range(folded.clone()..)
                .take_while(|(p, _)| p.starts_with(&folded))
                .map(|(_, p)| name(p))
                .collect()
        } else {
            self.roots
                .range(prefix.to_vec()..)
                .take_while(|(p, _)| p.starts_with(prefix))
                .map(|(p, _)| name(p))
                .collect()
        };
        if let Some(names) = self
            .lookup(prefix.strip_suffix(b"/").unwrap_or(prefix), insensitive)
            .filter(|node| node.tree)
            .and_then(|node| self.tree(&node.id))
        {
            result.extend(names.entries.keys().map(|p| bytes_path(p).into_os_string()));
        }
        result
    }
}
