//! Path-scoped exchange: `want --path [--range]`, `peek` and `read`.
//!
//! WHY THIS EXISTS. A paired computer must be able to show a chat that lives
//! on another computer without downloading that chat's project folder. The
//! chat's transcript is one file (`.cell/transcript.jsonl`) inside a snapshot
//! whose tree can hold gigabytes of project files. The whole-snapshot `want`
//! walks every tree and blob, so the only reader it allows is a take, which
//! pays for the entire folder. READING A REMOTE CHAT MUST NOT COST ITS PROJECT
//! FOLDER, so these verbs walk only the spine of one path: the snapshot, the
//! tree nodes (and tree pages) that name each component, the file's blob
//! index, and only the chunks covering the bytes the caller will show.
//!
//! WHY A RANGE, AND WHY IT STAYS CHEAP AS THE CHAT GROWS. Chunks are
//! content-defined (see [`crate::chunker`]): a boundary depends on the bytes
//! around it, not on its offset, so appending a turn to the transcript leaves
//! every earlier chunk byte-identical and keeps its id. A reader that already
//! holds a transcript's chunks and asks for the new tail range is therefore
//! told to fetch only the new tail chunk(s), plus the few small index objects
//! that changed (snapshot, trees on the path, blob).
//!
//! WHY THE OBJECTS LAND IN THE NORMAL STORE. Everything fetched for a read is
//! imported by the ordinary `import` into the ordinary object store, under the
//! ordinary ids. A later full `want` / `import` / `materialize` (a take)
//! consults the same store and so fetches only what is still missing: a later
//! take never pays twice for bytes a read already downloaded. The relay and
//! the encryption are untouched; these verbs only choose which existing
//! objects to ask for.
//!
//! DO NOT "SIMPLIFY" THIS INTO A FULL FETCH OR A MATERIALIZE. Either would make
//! opening a remote chat cost its whole folder, which is the exact cost this
//! module exists to avoid.

use super::fetch::WANT_LIMIT;
use super::survey::Object;
use crate::model::{id_hex, Blob, EntryKind, ObjectId, ObjectKind, Snapshot, Tree, TreeEntry};
use crate::sealer::Sealer;
use crate::store::ObjectStore;
use anyhow::Context;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fmt;
use std::io::Write;
use std::path::Path;

/// A refusal a caller acts on by its `code`. Its text is ONE JSON object, so
/// the command line's stderr and the daemon's `err` string carry the same
/// machine-readable answer and a caller can match `"code":"incomplete"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathError {
    /// `incomplete`, `not_found`, `not_a_file` or `bad_range`.
    pub code: &'static str,
    pub path: String,
    pub message: String,
    /// For `incomplete`: the kind of the first object not held.
    pub kind: Option<ObjectKind>,
    /// For `incomplete`, when keys were given: that object's remote id.
    pub want: Option<String>,
}

impl PathError {
    fn new(code: &'static str, path: &[u8], message: impl Into<String>) -> Self {
        Self {
            code,
            path: String::from_utf8_lossy(path).into_owned(),
            message: message.into(),
            kind: None,
            want: None,
        }
    }

    fn incomplete(path: &[u8], (kind, id): Object, sealer: Option<&dyn Sealer>) -> Self {
        let mut error = Self::new(
            "incomplete",
            path,
            format!(
                "the store does not hold {} {}",
                kind_word(kind),
                id_hex(&id)
            ),
        );
        error.kind = Some(kind);
        error.want = sealer.map(|sealer| id_hex(&sealer.remote_id(kind, &id)));
        error
    }

    pub fn to_json(&self) -> Value {
        let mut value = json!({"code": self.code, "path": self.path, "message": self.message});
        if let Some(kind) = self.kind {
            value["kind"] = json!(kind_word(kind));
        }
        if let Some(want) = &self.want {
            value["want"] = json!(want);
        }
        value
    }
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_json())
    }
}

impl std::error::Error for PathError {}

fn kind_word(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Chunk => "chunk",
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Snapshot => "snapshot",
        ObjectKind::Xattrs => "xattrs",
    }
}

/// A byte range `[start, end)` of one file. `end: None` means to the end of
/// the file; an `end` past the file is clamped to its length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: Option<u64>,
}

impl ByteRange {
    /// Parses `S:E` or `S:` (to the end of the file).
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let bad = || PathError::new("bad_range", b"", format!("range {text:?} is not S:E"));
        let (start, end) = text.split_once(':').ok_or_else(bad)?;
        let start = start.trim().parse::<u64>().map_err(|_| bad())?;
        let end = match end.trim() {
            "" => None,
            end => Some(end.parse::<u64>().map_err(|_| bad())?),
        };
        if end.is_some_and(|end| end < start) {
            return Err(bad().into());
        }
        Ok(Self { start, end })
    }

    /// The range against a file of `len` bytes, or `bad_range` when it starts
    /// past the end.
    fn clamp(self, len: u64, path: &[u8]) -> Result<(u64, u64), PathError> {
        if self.start > len {
            return Err(PathError::new(
                "bad_range",
                path,
                format!("range starts at {} past the file's {len} bytes", self.start),
            ));
        }
        Ok((self.start, self.end.map_or(len, |end| end.min(len))))
    }
}

/// What one path names in a snapshot, as far as the held objects can tell.
enum Located {
    /// The walk stopped at an object the store does not hold.
    Missing(Object),
    /// The path is the snapshot's root directory.
    Root(ObjectId),
    Entry(TreeEntry),
}

/// One lookup step inside a directory, following its pages.
enum Lookup {
    Missing(Object),
    Absent,
    Found(TreeEntry),
}

/// The path's components. An empty path, `.` or `/` names the root; `.cell`
/// is an ordinary directory entry of the sealed tree, because a composed
/// overlay is sealed as the tree's `.cell` entry (see [`crate::overlay`]).
fn components(path: &[u8]) -> anyhow::Result<Vec<&[u8]>> {
    let trimmed = path.strip_prefix(b"./").unwrap_or(path);
    let trimmed = trimmed.strip_prefix(b"/").unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix(b"/").unwrap_or(trimmed);
    if trimmed.is_empty() || trimmed == b"." {
        return Ok(Vec::new());
    }
    let parts: Vec<&[u8]> = trimmed.split(|byte| *byte == b'/').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == b"." || *part == b"..")
    {
        return Err(
            PathError::new("not_found", path, "the path is not a plain relative path").into(),
        );
    }
    Ok(parts)
}

/// Finds `name` in the directory tree `tree`, descending its pages. Only the
/// one page whose name range covers `name` is read at each level: a
/// directory with a hundred thousand entries costs one page per level, not
/// the whole listing.
fn lookup(store: &ObjectStore, mut tree: ObjectId, name: &[u8]) -> anyhow::Result<Lookup> {
    loop {
        if !store.contains_object(&tree)? {
            return Ok(Lookup::Missing((ObjectKind::Tree, tree)));
        }
        let node: Tree = store.read_struct(&tree, ObjectKind::Tree)?;
        if node.pages.is_empty() {
            return Ok(node
                .entries
                .binary_search_by(|entry| entry.name.as_slice().cmp(name))
                .map_or(Lookup::Absent, |index| {
                    Lookup::Found(node.entries[index].clone())
                }));
        }
        let Some(page) = node
            .pages
            .iter()
            .find(|page| page.first_name.as_slice() <= name && name <= page.last_name.as_slice())
        else {
            return Ok(Lookup::Absent);
        };
        tree = page.target;
    }
}

/// Walks only the spine of `path`: snapshot, then one tree node (or page) per
/// level. Nothing off the path is read or asked for — that is the whole
/// point of the path-scoped verbs (see the module comment).
fn locate(store: &ObjectStore, head: ObjectId, path: &[u8]) -> anyhow::Result<Located> {
    let parts = components(path)?;
    if !store.contains_object(&head)? {
        return Ok(Located::Missing((ObjectKind::Snapshot, head)));
    }
    let snapshot: Snapshot = store.read_struct(&head, ObjectKind::Snapshot)?;
    let mut dir = snapshot.root_tree;
    let Some((last, parents)) = parts.split_last() else {
        return Ok(Located::Root(dir));
    };
    for part in parents {
        match lookup(store, dir, part)? {
            Lookup::Missing(object) => return Ok(Located::Missing(object)),
            Lookup::Absent => return Err(not_found(path)),
            Lookup::Found(entry) => match (entry.kind, entry.target) {
                (EntryKind::Directory, Some(target)) => dir = target,
                _ => return Err(not_found(path)),
            },
        }
    }
    match lookup(store, dir, last)? {
        Lookup::Missing(object) => Ok(Located::Missing(object)),
        Lookup::Absent => Err(not_found(path)),
        Lookup::Found(entry) => Ok(Located::Entry(entry)),
    }
}

fn not_found(path: &[u8]) -> anyhow::Error {
    PathError::new("not_found", path, "the snapshot has no such path").into()
}

/// The file a range verb works on: its blob id, or `None` for an empty file.
fn file_target(located: &Located, path: &[u8]) -> Result<Option<ObjectId>, PathError> {
    match located {
        Located::Entry(entry) if entry.kind == EntryKind::File => Ok(entry.target),
        _ => Err(PathError::new("not_a_file", path, "the path is not a file")),
    }
}

/// The chunks of `blob` overlapping `[start, end)`, with their offsets. An
/// empty range overlaps nothing.
fn overlapping(blob: &Blob, start: u64, end: u64) -> Vec<(u64, u64, ObjectId)> {
    let mut offset = 0_u64;
    let mut found = Vec::new();
    for chunk in &blob.chunks {
        let chunk_end = offset + u64::from(chunk.len);
        if start < end && offset < end && start < chunk_end {
            found.push((offset, chunk_end, chunk.id));
        }
        offset = chunk_end;
    }
    found
}

/// The missing tree nodes of one directory (its node and every page below
/// it), for a directory listing. Its entries' own targets are not wanted.
fn missing_pages(store: &ObjectStore, tree: ObjectId, limit: usize) -> anyhow::Result<Vec<Object>> {
    let mut missing = Vec::new();
    let mut queue = VecDeque::from([tree]);
    while let Some(id) = queue.pop_front() {
        if missing.len() >= limit {
            break;
        }
        if !store.contains_object(&id)? {
            missing.push((ObjectKind::Tree, id));
            continue;
        }
        let node: Tree = store.read_struct(&id, ObjectKind::Tree)?;
        queue.extend(node.pages.iter().map(|page| page.target));
    }
    Ok(missing)
}

/// `want --head H --path P [--range S:E]`: the remote ids needed next to read
/// `path` — and, with a range, those bytes of it — out of `head`, nearest the
/// snapshot first, at most [`WANT_LIMIT`]. Empty means everything that scope
/// needs is held.
///
/// Like the whole-snapshot `want`, an object that is not held cannot be
/// opened, so the answer is the next frontier only; a caller loops
/// want → fetch → import until the answer is empty. The scope is the path's
/// spine, never the rest of the snapshot, because reading a remote chat must
/// not cost its project folder. With a range, only the chunks overlapping it
/// are named: content-defined chunking keeps a growing transcript's earlier
/// chunks identical, so re-wanting the tail after a new turn names only the
/// new tail chunk(s). The objects are imported into the normal store, so a
/// later take reuses them rather than downloading them again.
pub fn want_path(
    store: &ObjectStore,
    sealer: &dyn Sealer,
    head: ObjectId,
    path: &[u8],
    range: Option<ByteRange>,
) -> anyhow::Result<Vec<ObjectId>> {
    let _section = store.acquire_maintenance_shared()?;
    let rid = |(kind, id): Object| sealer.remote_id(kind, &id);
    let located = locate(store, head, path)?;
    if let Located::Missing(object) = located {
        return Ok(vec![rid(object)]);
    }
    let Some(range) = range else {
        return Ok(match located {
            Located::Root(tree) => missing_pages(store, tree, WANT_LIMIT)?,
            Located::Entry(entry) => match (entry.kind, entry.target) {
                (EntryKind::Directory, Some(tree)) => missing_pages(store, tree, WANT_LIMIT)?,
                (EntryKind::File, Some(blob)) if !store.contains_object(&blob)? => {
                    vec![(ObjectKind::Blob, blob)]
                }
                _ => Vec::new(),
            },
            Located::Missing(_) => unreachable!("handled above"),
        }
        .into_iter()
        .map(rid)
        .collect());
    };
    let Some(blob_id) = file_target(&located, path)? else {
        range.clamp(0, path)?;
        return Ok(Vec::new());
    };
    if !store.contains_object(&blob_id)? {
        return Ok(vec![rid((ObjectKind::Blob, blob_id))]);
    }
    let blob: Blob = store.read_struct(&blob_id, ObjectKind::Blob)?;
    let (start, end) = range.clamp(blob.total_len, path)?;
    let mut wanted = Vec::new();
    for (_, _, chunk) in overlapping(&blob, start, end) {
        if wanted.len() >= WANT_LIMIT {
            break;
        }
        if !store.contains_object(&chunk)? && !wanted.contains(&chunk) {
            wanted.push(chunk);
        }
    }
    Ok(wanted
        .into_iter()
        .map(|chunk| rid((ObjectKind::Chunk, chunk)))
        .collect())
}

/// `peek --head H --path P`: what the held objects say about one path, never
/// touching the network. A file answers its length and its chunks' byte
/// ranges with whether each is held, so a reader can decide which range to
/// want before paying for it; a directory answers its entries. An index
/// object on the path that is not held is a `"code":"incomplete"` refusal.
///
/// It exists so opening a remote chat can learn the transcript's size and
/// layout from a few KB of index objects instead of the project folder.
pub fn peek(
    store: &ObjectStore,
    sealer: Option<&dyn Sealer>,
    head: ObjectId,
    path: &[u8],
) -> anyhow::Result<Value> {
    let _section = store.acquire_maintenance_shared()?;
    let tree = match locate(store, head, path)? {
        Located::Missing(object) => return Err(PathError::incomplete(path, object, sealer).into()),
        Located::Root(tree) => tree,
        Located::Entry(entry) => match (entry.kind, entry.target) {
            (EntryKind::Directory, Some(tree)) => tree,
            (EntryKind::File, None) => {
                return Ok(json!({"kind": "file", "len": 0, "chunks": []}));
            }
            (EntryKind::File, Some(blob_id)) => {
                if !store.contains_object(&blob_id)? {
                    return Err(
                        PathError::incomplete(path, (ObjectKind::Blob, blob_id), sealer).into(),
                    );
                }
                let blob: Blob = store.read_struct(&blob_id, ObjectKind::Blob)?;
                let mut chunks = Vec::with_capacity(blob.chunks.len());
                for (start, end, chunk) in overlapping(&blob, 0, blob.total_len) {
                    let held = store.contains_object(&chunk)?;
                    chunks.push(json!({"start": start, "end": end, "held": held}));
                }
                return Ok(json!({"kind": "file", "len": blob.total_len, "chunks": chunks}));
            }
            (kind, _) => return Ok(json!({"kind": entry_word(&kind), "len": entry.size})),
        },
    };
    let mut entries = Vec::new();
    list(store, sealer, path, tree, &mut entries)?;
    Ok(json!({"kind": "dir", "entries": entries}))
}

/// Appends a directory's entries in name order, following its pages.
fn list(
    store: &ObjectStore,
    sealer: Option<&dyn Sealer>,
    path: &[u8],
    tree: ObjectId,
    into: &mut Vec<Value>,
) -> anyhow::Result<()> {
    if !store.contains_object(&tree)? {
        return Err(PathError::incomplete(path, (ObjectKind::Tree, tree), sealer).into());
    }
    let node: Tree = store.read_struct(&tree, ObjectKind::Tree)?;
    for page in &node.pages {
        list(store, sealer, path, page.target, into)?;
    }
    for entry in &node.entries {
        into.push(json!({
            "name": String::from_utf8_lossy(&entry.name),
            "kind": entry_word(&entry.kind),
            "len": entry.size,
        }));
    }
    Ok(())
}

fn entry_word(kind: &EntryKind) -> &'static str {
    match kind {
        EntryKind::File => "file",
        EntryKind::Directory => "dir",
        EntryKind::Symlink => "symlink",
        EntryKind::Fifo | EntryKind::SocketMarker => "other",
    }
}

/// `read --head H --path P --range S:E --out FILE`: writes exactly bytes
/// `[S, E)` of one file (E clamped to its length) from held chunks into
/// `out`, and answers `{"bytes": n}`. It restores nothing else — no tree, no
/// workspace, no head moves — because showing a remote chat must not
/// materialize its project. Any chunk of the range not held is a
/// `"code":"incomplete"` refusal and `out` is left untouched; the caller
/// wants that range first. The output is written beside `out` and renamed
/// into place, so a reader never sees half a range.
pub fn read(
    store: &ObjectStore,
    sealer: Option<&dyn Sealer>,
    head: ObjectId,
    path: &[u8],
    range: ByteRange,
    out: &Path,
) -> anyhow::Result<Value> {
    let _section = store.acquire_maintenance_shared()?;
    let located = locate(store, head, path)?;
    if let Located::Missing(object) = located {
        return Err(PathError::incomplete(path, object, sealer).into());
    }
    let blob = match file_target(&located, path)? {
        None => Blob {
            chunks: Vec::new(),
            total_len: 0,
        },
        Some(blob_id) => {
            if !store.contains_object(&blob_id)? {
                return Err(
                    PathError::incomplete(path, (ObjectKind::Blob, blob_id), sealer).into(),
                );
            }
            store.read_struct(&blob_id, ObjectKind::Blob)?
        }
    };
    let (start, end) = range.clamp(blob.total_len, path)?;
    let pieces = overlapping(&blob, start, end);
    for (_, _, chunk) in &pieces {
        if !store.contains_object(chunk)? {
            return Err(PathError::incomplete(path, (ObjectKind::Chunk, *chunk), sealer).into());
        }
    }
    let dir = out
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut staged =
        tempfile::NamedTempFile::new_in(dir).with_context(|| format!("stage {}", out.display()))?;
    let mut written = 0_u64;
    for (chunk_start, _, chunk) in pieces {
        let bytes = store.read_bytes(&chunk, ObjectKind::Chunk)?;
        let from = start.saturating_sub(chunk_start) as usize;
        let to = (end - chunk_start).min(bytes.len() as u64) as usize;
        staged.write_all(&bytes[from..to])?;
        written += (to - from) as u64;
    }
    staged.as_file().sync_all()?;
    staged
        .persist(out)
        .with_context(|| format!("write {}", out.display()))?;
    debug_assert_eq!(written, end - start);
    Ok(json!({"bytes": written}))
}
