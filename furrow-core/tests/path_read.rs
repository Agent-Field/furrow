//! Path-scoped exchange: `want --path [--range]`, `peek` and `read`.
//!
//! The guarantee these tests hold: reading one file of a snapshot held on a
//! relay (a chat's `.cell/transcript.jsonl`) downloads the spine of that path
//! and the chunks of the bytes shown — never the project folder beside it —
//! and whatever it downloaded is reused by a later full take, never fetched
//! twice.

mod common;

use common::*;
use furrow::exchange::materialize::materialize;
use furrow::exchange::open_store;
use furrow::exchange::path::{peek, read, want_path, ByteRange, PathError};
use furrow::model::{Blob, ObjectId, ObjectKind, Snapshot, SnapshotTrigger, Tree};
use furrow::repository::{FurrowRepository, SealOptions};
use furrow::sealer::Sealer;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::PathBuf;

const TRANSCRIPT: &str = ".cell/transcript.jsonl";
const FRAME: usize = 300 * 1024;

/// A chat cell as codeaf seals it: a project tree with large files, and the
/// cell directory composed in as the tree's `.cell/` entry.
struct Chat {
    temp: tempfile::TempDir,
    data: PathBuf,
    tree: PathBuf,
    cell: PathBuf,
    head: ObjectId,
}

/// Transcript-like text: JSON lines whose bytes do not repeat, so the
/// chunker finds its ordinary content-defined boundaries.
fn transcript_lines(from: usize, count: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1d_u64 ^ from as u64;
    let mut out = Vec::new();
    for n in from..from + count {
        let mut word = String::new();
        for _ in 0..40 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            word.push(char::from(b'a' + (state % 26) as u8));
        }
        out.extend_from_slice(format!("{{\"turn\":{n},\"text\":\"{word}\"}}\n").as_bytes());
    }
    out
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

fn chat() -> Chat {
    let temp = tempfile::tempdir().unwrap();
    let (data, tree, cell) = (
        temp.path().join("data-a"),
        temp.path().join("tree-a"),
        temp.path().join("cell-a"),
    );
    fs::create_dir_all(tree.join("src")).unwrap();
    fs::write(tree.join("project.bin"), noise(4 << 20, 7)).unwrap();
    fs::write(tree.join("src/lib.rs"), b"pub fn f() {}\n").unwrap();
    fs::create_dir_all(cell.join("receipts")).unwrap();
    fs::write(cell.join("transcript.jsonl"), transcript_lines(0, 40_000)).unwrap();
    fs::write(cell.join("meta.json"), b"{\"v\":1}").unwrap();
    fs::write(cell.join("receipts/1.json"), b"{\"turn\":1}").unwrap();
    let options = SealOptions {
        changed: None,
        overlay: Some(cell.clone()),
    };
    let (_, head) =
        FurrowRepository::attach_and_seal_in(&data, &tree, None, SnapshotTrigger::Manual, options)
            .unwrap();
    Chat {
        temp,
        data,
        tree,
        cell,
        head,
    }
}

impl Chat {
    /// Seals what the folders hold now and returns the new head.
    fn reseal(&mut self) -> ObjectId {
        let mut repo =
            FurrowRepository::open_composed_in(&self.data, &self.tree, Some(self.cell.clone()))
                .unwrap();
        self.head = repo.snapshot(None, SnapshotTrigger::Manual).unwrap();
        self.head
    }

    /// Every object of the head as the relay holds it: sealed bytes by rid.
    fn relay(&self) -> HashMap<ObjectId, Vec<u8>> {
        let outbox = self.temp.path().join(format!("outbox-{}", hex(&self.head)));
        objects_of(&frame_paths(&export(
            &Source {
                _temp: tempfile::tempdir().unwrap(),
                data: self.data.clone(),
                tree: self.tree.clone(),
                cell: self.cell.clone(),
                head: self.head,
            },
            LEDGER,
            &outbox,
            FRAME,
        )))
        .into_iter()
        .collect()
    }

    fn transcript(&self) -> Vec<u8> {
        fs::read(self.cell.join("transcript.jsonl")).unwrap()
    }

    /// The transcript's blob as the source store holds it.
    fn blob(&self, path: &str) -> Blob {
        let store = open_store(&self.data).unwrap();
        let snapshot: Snapshot = store.read_struct(&self.head, ObjectKind::Snapshot).unwrap();
        let entry = furrow::tree::find_path(&store, &snapshot.root_tree, path.as_bytes())
            .unwrap()
            .unwrap();
        store
            .read_struct(&entry.target.unwrap(), ObjectKind::Blob)
            .unwrap()
    }
}

fn hex(id: &ObjectId) -> String {
    furrow::model::id_hex(id)
}

/// A reading computer: its own store and inbox, and a tally of what it pulled
/// from the relay.
struct Reader {
    _temp: tempfile::TempDir,
    data: PathBuf,
    inbox: PathBuf,
    fetched: Vec<ObjectId>,
}

fn reader() -> Reader {
    let temp = tempfile::tempdir().unwrap();
    Reader {
        data: temp.path().join("data-b"),
        inbox: temp.path().join("inbox"),
        fetched: Vec::new(),
        _temp: temp,
    }
}

impl Reader {
    fn want(&self, head: ObjectId, path: &str, range: Option<&str>) -> Vec<ObjectId> {
        let store = open_store(&self.data).unwrap();
        want_path(
            &store,
            &sealer(),
            head,
            path.as_bytes(),
            range.map(|range| ByteRange::parse(range).unwrap()),
        )
        .unwrap()
    }

    /// One round: deliver `rids` from the relay and run the ordinary strict
    /// import, which must accept path-scoped objects without the rest of the
    /// graph.
    fn take(&mut self, relay: &HashMap<ObjectId, Vec<u8>>, head: ObjectId, rids: &[ObjectId]) {
        let objects: Vec<_> = rids.iter().map(|rid| (*rid, relay[rid].clone())).collect();
        put_inbox(&self.inbox, &objects);
        let imported = import_all(&self.data, head, &self.inbox).unwrap();
        assert_eq!(
            imported,
            rids.len(),
            "strict import takes every path object"
        );
        self.fetched.extend_from_slice(rids);
    }

    /// The want → fetch → import loop a caller runs, until the scope is held.
    fn fetch(
        &mut self,
        relay: &HashMap<ObjectId, Vec<u8>>,
        head: ObjectId,
        path: &str,
        range: Option<&str>,
    ) -> Vec<ObjectId> {
        let mut pulled = Vec::new();
        loop {
            let rids = self.want(head, path, range);
            if rids.is_empty() {
                return pulled;
            }
            self.take(relay, head, &rids);
            pulled.extend(rids);
        }
    }

    /// The whole-snapshot loop a take runs.
    fn fetch_all(&mut self, relay: &HashMap<ObjectId, Vec<u8>>, head: ObjectId) -> Vec<ObjectId> {
        let mut pulled = Vec::new();
        loop {
            let rids = want_of(&self.data, head);
            if rids.is_empty() {
                return pulled;
            }
            self.take(relay, head, &rids);
            pulled.extend(rids);
        }
    }

    fn peek(&self, head: ObjectId, path: &str) -> anyhow::Result<serde_json::Value> {
        let store = open_store(&self.data).unwrap();
        peek(&store, Some(&sealer()), head, path.as_bytes())
    }

    fn read(&self, head: ObjectId, path: &str, range: &str) -> anyhow::Result<Vec<u8>> {
        let store = open_store(&self.data).unwrap();
        let out = self._temp.path().join("out.bin");
        let _ = fs::remove_file(&out);
        let answer = read(
            &store,
            Some(&sealer()),
            head,
            path.as_bytes(),
            ByteRange::parse(range)?,
            &out,
        )?;
        let bytes = fs::read(&out).unwrap();
        assert_eq!(answer["bytes"], bytes.len() as u64);
        Ok(bytes)
    }
}

fn chunk_rids(blob: &Blob, indexes: &[usize]) -> BTreeSet<ObjectId> {
    indexes
        .iter()
        .map(|&i| sealer().remote_id(ObjectKind::Chunk, &blob.chunks[i].id))
        .collect()
}

fn offsets(blob: &Blob) -> Vec<(u64, u64)> {
    let mut at = 0;
    blob.chunks
        .iter()
        .map(|chunk| {
            let start = at;
            at += u64::from(chunk.len);
            (start, at)
        })
        .collect()
}

fn code_of(error: &anyhow::Error) -> &'static str {
    error
        .downcast_ref::<PathError>()
        .unwrap_or_else(|| panic!("not a path refusal: {error:#}"))
        .code
}

/// The kinds of a set of rids, from the relay's own full listing.
fn kinds_of(chat: &Chat, rids: &[ObjectId]) -> Vec<ObjectKind> {
    let all = kinds(&chat.data, chat.head);
    rids.iter().map(|rid| all[rid]).collect()
}

#[test]
fn a_path_want_walks_only_the_spine_of_that_path() {
    let chat = chat();
    let relay = chat.relay();
    let mut b = reader();
    let pulled = b.fetch(&relay, chat.head, TRANSCRIPT, None);

    // Snapshot, root tree, `.cell` tree, the transcript's blob: nothing else.
    let mut kinds = kinds_of(&chat, &pulled);
    kinds.sort_by_key(|kind| *kind as u8);
    assert_eq!(
        kinds,
        [
            ObjectKind::Blob,
            ObjectKind::Tree,
            ObjectKind::Tree,
            ObjectKind::Snapshot
        ],
        "only the path's index objects; no chunk, no project file"
    );
    let project = chat.blob("project.bin");
    let store = open_store(&b.data).unwrap();
    assert!(
        project
            .chunks
            .iter()
            .all(|chunk| store.read_bytes(&chunk.id, ObjectKind::Chunk).is_err()),
        "the project folder was not downloaded"
    );

    // peek learns the layout from those few objects, and holds no chunk yet.
    let blob = chat.blob(TRANSCRIPT);
    let answer = b.peek(chat.head, TRANSCRIPT).unwrap();
    assert_eq!(answer["kind"], "file");
    assert_eq!(answer["len"], chat.transcript().len() as u64);
    let chunks = answer["chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), blob.chunks.len());
    assert!(blob.chunks.len() >= 4, "the fixture spans several chunks");
    for (chunk, (start, end)) in chunks.iter().zip(offsets(&blob)) {
        assert_eq!(
            (chunk["start"].as_u64(), chunk["end"].as_u64()),
            (Some(start), Some(end))
        );
        assert_eq!(chunk["held"], false);
    }
}

#[test]
fn a_range_wants_exactly_the_chunks_it_overlaps() {
    let chat = chat();
    let relay = chat.relay();
    let mut b = reader();
    b.fetch(&relay, chat.head, TRANSCRIPT, None);
    let blob = chat.blob(TRANSCRIPT);
    let spans = offsets(&blob);
    let len = blob.total_len;
    let last = spans.len() - 1;
    let (s1, e1) = spans[1];
    let want = |b: &Reader, range: &str| -> BTreeSet<ObjectId> {
        b.want(chat.head, TRANSCRIPT, Some(range))
            .into_iter()
            .collect()
    };

    // Inside one chunk.
    assert_eq!(
        want(&b, &format!("{}:{}", s1 + 10, e1 - 10)),
        chunk_rids(&blob, &[1])
    );
    // Exactly one chunk, end exclusive: the next chunk is not touched.
    assert_eq!(want(&b, &format!("{s1}:{e1}")), chunk_rids(&blob, &[1]));
    // One byte across a boundary spans both chunks.
    assert_eq!(
        want(&b, &format!("{}:{}", e1 - 1, e1 + 1)),
        chunk_rids(&blob, &[1, 2])
    );
    // From inside chunk 0 to inside chunk 2.
    assert_eq!(
        want(&b, &format!("5:{}", spans[2].0 + 3)),
        chunk_rids(&blob, &[0, 1, 2])
    );
    // The end of the file, spelled both ways, and an end past it (clamped).
    assert_eq!(
        want(&b, &format!("{}:{len}", len - 10)),
        chunk_rids(&blob, &[last])
    );
    assert_eq!(
        want(&b, &format!("{}:", len - 10)),
        chunk_rids(&blob, &[last])
    );
    assert_eq!(
        want(&b, &format!("{}:{}", len - 10, len + 999)),
        chunk_rids(&blob, &[last])
    );
    // An empty range, including one at the very end, wants nothing.
    assert!(want(&b, &format!("{s1}:{s1}")).is_empty());
    assert!(want(&b, &format!("{len}:{len}")).is_empty());
    // A range starting past the end is refused, not silently empty.
    let store = open_store(&b.data).unwrap();
    let error = want_path(
        &store,
        &sealer(),
        chat.head,
        TRANSCRIPT.as_bytes(),
        Some(ByteRange::parse(&format!("{}:", len + 1)).unwrap()),
    )
    .unwrap_err();
    assert_eq!(code_of(&error), "bad_range");
    assert!(ByteRange::parse("9:3").is_err());
    assert!(ByteRange::parse("x").is_err());

    // Held chunks are not wanted again.
    let tail = format!("{}:", len - 10);
    b.fetch(&relay, chat.head, TRANSCRIPT, Some(&tail));
    assert!(want(&b, &tail).is_empty());
    assert_eq!(
        want(&b, &format!("{}:", e1 - 1)),
        chunk_rids(&blob, &(1..last).collect::<Vec<_>>())
    );
}

#[test]
fn a_small_file_is_one_chunk_and_reads_byte_for_byte() {
    let chat = chat();
    let relay = chat.relay();
    let mut b = reader();
    let pulled = b.fetch(&relay, chat.head, ".cell/meta.json", Some("0:"));
    assert_eq!(
        kinds_of(&chat, &pulled)
            .iter()
            .filter(|kind| **kind == ObjectKind::Chunk)
            .count(),
        1
    );
    assert_eq!(
        b.read(chat.head, ".cell/meta.json", "0:").unwrap(),
        b"{\"v\":1}"
    );
    assert_eq!(
        b.read(chat.head, ".cell/meta.json", "2:5").unwrap(),
        b"v\":"
    );
    assert_eq!(b.read(chat.head, ".cell/meta.json", "3:3").unwrap(), b"");
}

#[test]
fn read_writes_exactly_the_range_and_refuses_a_chunk_it_lacks() {
    let chat = chat();
    let relay = chat.relay();
    let mut b = reader();
    let text = chat.transcript();
    let len = text.len();
    let blob = chat.blob(TRANSCRIPT);
    let spans = offsets(&blob);

    let tail = format!("{}:{len}", len - 300_000);
    b.fetch(&relay, chat.head, TRANSCRIPT, Some(&tail));
    assert_eq!(
        b.read(chat.head, TRANSCRIPT, &tail).unwrap(),
        &text[len - 300_000..]
    );
    let (s, e) = spans[spans.len() - 1];
    let inner = format!("{}:{}", s + 1, e - 1);
    assert_eq!(
        b.read(chat.head, TRANSCRIPT, &inner).unwrap(),
        &text[s as usize + 1..e as usize - 1]
    );

    // The head of the file is not held: an incomplete refusal that names the
    // chunk to want, and no output file.
    let out = b._temp.path().join("refused.bin");
    let store = open_store(&b.data).unwrap();
    let error = read(
        &store,
        Some(&sealer()),
        chat.head,
        TRANSCRIPT.as_bytes(),
        ByteRange::parse("0:10").unwrap(),
        &out,
    )
    .unwrap_err();
    let refusal = error.downcast_ref::<PathError>().unwrap();
    assert_eq!(refusal.code, "incomplete");
    assert_eq!(refusal.kind, Some(ObjectKind::Chunk));
    assert_eq!(
        refusal.want.as_deref(),
        Some(hex(&sealer().remote_id(ObjectKind::Chunk, &blob.chunks[0].id)).as_str())
    );
    let text_of_error = error.to_string();
    assert!(
        text_of_error.contains("\"code\":\"incomplete\""),
        "{text_of_error}"
    );
    assert!(!out.exists());

    // Whole file once everything is wanted.
    b.fetch(&relay, chat.head, TRANSCRIPT, Some("0:"));
    assert_eq!(b.read(chat.head, TRANSCRIPT, "0:").unwrap(), text);
}

#[test]
fn peek_and_read_refuse_before_the_index_is_held() {
    let chat = chat();
    let relay = chat.relay();
    let mut b = reader();
    let error = b.peek(chat.head, TRANSCRIPT).unwrap_err();
    assert_eq!(code_of(&error), "incomplete");
    assert_eq!(
        error.downcast_ref::<PathError>().unwrap().kind,
        Some(ObjectKind::Snapshot)
    );
    // With only the snapshot, the next missing object is a tree.
    let first = b.want(chat.head, TRANSCRIPT, None);
    b.take(&relay, chat.head, &first);
    let error = b.peek(chat.head, TRANSCRIPT).unwrap_err();
    assert_eq!(
        error.downcast_ref::<PathError>().unwrap().kind,
        Some(ObjectKind::Tree)
    );
    assert!(b.read(chat.head, TRANSCRIPT, "0:1").is_err());
}

#[test]
fn peek_lists_a_directory_and_refuses_a_missing_path() {
    let chat = chat();
    let relay = chat.relay();
    let mut b = reader();
    b.fetch(&relay, chat.head, ".cell", None);
    let answer = b.peek(chat.head, ".cell").unwrap();
    assert_eq!(answer["kind"], "dir");
    let names: Vec<_> = answer["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["name"].as_str().unwrap().to_owned(),
                entry["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        names,
        [
            ("meta.json".to_owned(), "file".to_owned()),
            ("receipts".to_owned(), "dir".to_owned()),
            ("transcript.jsonl".to_owned(), "file".to_owned())
        ]
    );
    let root = b.peek(chat.head, "").unwrap();
    assert!(root["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["name"] == ".cell"));

    let store = open_store(&b.data).unwrap();
    let missing = want_path(&store, &sealer(), chat.head, b".cell/nope.jsonl", None).unwrap_err();
    assert_eq!(code_of(&missing), "not_found");
    let ranged_dir = want_path(
        &store,
        &sealer(),
        chat.head,
        b".cell",
        Some(ByteRange::parse("0:1").unwrap()),
    )
    .unwrap_err();
    assert_eq!(code_of(&ranged_dir), "not_a_file");
}

/// A new turn appends to the transcript. Content-defined chunks keep every
/// earlier chunk identical, so re-reading the tail of the new head downloads
/// only the index objects that changed and the new tail chunk(s).
#[test]
fn an_appended_turn_rewants_only_the_new_tail() {
    let mut chat = chat();
    let relay = chat.relay();
    let mut b = reader();
    b.fetch(&relay, chat.head, TRANSCRIPT, Some("0:"));
    let old = chat.blob(TRANSCRIPT);

    let mut text = chat.transcript();
    text.extend(transcript_lines(40_000, 400));
    fs::write(chat.cell.join("transcript.jsonl"), &text).unwrap();
    let head = chat.reseal();
    let relay = chat.relay();
    let new = chat.blob(TRANSCRIPT);

    let kept = new
        .chunks
        .iter()
        .filter(|chunk| old.chunks.contains(chunk))
        .count();
    assert!(
        kept >= old.chunks.len() - 1,
        "every chunk but the old tail is unchanged"
    );

    let tail = format!("{}:", text.len() - 1000);
    let pulled = b.fetch(&relay, head, TRANSCRIPT, Some(&tail));
    let chunks = kinds_of(&chat, &pulled)
        .iter()
        .filter(|kind| **kind == ObjectKind::Chunk)
        .count();
    let fresh = new.chunks.len() - kept;
    assert!(
        chunks <= fresh && chunks >= 1,
        "{chunks} chunks for {fresh} new ones"
    );
    assert!(
        pulled.len() <= chunks + 4,
        "index objects: snapshot, two trees, blob"
    );
    // The rest of the new file is already held, except new chunks before the tail.
    let rest = b.fetch(&relay, head, TRANSCRIPT, Some("0:"));
    assert_eq!(chunks + rest.len(), fresh, "never an old chunk again");
    assert_eq!(b.read(head, TRANSCRIPT, "0:").unwrap(), text);
}

/// Objects fetched for reading land in the normal store: verify and gc do not
/// trip over the partial graph, and a later full take downloads only what is
/// missing and restores byte-identically.
#[test]
fn a_later_take_never_pays_twice() {
    let chat = chat();
    let relay = chat.relay();
    let mut b = reader();
    let tail = format!("{}:", chat.transcript().len() - 1000);
    b.fetch(&relay, chat.head, TRANSCRIPT, Some(&tail));
    let read_first: BTreeSet<_> = b.fetched.iter().copied().collect();

    let store = open_store(&b.data).unwrap();
    store.verify_index(true).unwrap();
    drop(store);

    let rest = b.fetch_all(&relay, chat.head);
    assert!(
        rest.iter().all(|rid| !read_first.contains(rid)),
        "the take fetched an object the read already held"
    );
    assert_eq!(
        read_first.len() + rest.len(),
        relay.len(),
        "together: every object once"
    );

    let target = b._temp.path().join("tree-b");
    let cell = b._temp.path().join("cell-b");
    fs::create_dir_all(&target).unwrap();
    materialize(&b.data, &target, Some(cell.clone()), chat.head).unwrap();
    assert_eq!(
        fs::read(cell.join("transcript.jsonl")).unwrap(),
        chat.transcript()
    );
    assert_eq!(
        fs::read(target.join("project.bin")).unwrap(),
        fs::read(chat.tree.join("project.bin")).unwrap()
    );
}

#[test]
fn gc_on_a_partial_store_does_not_error_and_a_fetch_recovers() {
    let chat = chat();
    let relay = chat.relay();
    let mut b = reader();
    b.fetch(&relay, chat.head, TRANSCRIPT, Some("0:1000"));
    let mut store = open_store(&b.data).unwrap();
    furrow::gc::collect(&mut store, false).unwrap();
    store.verify_index(true).unwrap();
    drop(store);
    // Whatever gc kept or collected, the scope can be completed again.
    b.fetch(&relay, chat.head, TRANSCRIPT, Some("0:1000"));
    assert_eq!(
        b.read(chat.head, TRANSCRIPT, "0:1000").unwrap(),
        &chat.transcript()[..1000]
    );
}

/// A directory large enough to be paged: the walk reads only the page that
/// covers the name, not every page of the listing.
#[test]
fn a_path_through_a_paged_directory_reads_one_page_per_level() {
    let temp = tempfile::tempdir().unwrap();
    let (data, tree) = (temp.path().join("data"), temp.path().join("tree"));
    fs::create_dir_all(tree.join("many")).unwrap();
    for n in 0..4000 {
        fs::write(
            tree.join(format!("many/file-{n:05}-with-a-longer-name.txt")),
            format!("{n}\n"),
        )
        .unwrap();
    }
    let (_, head) = FurrowRepository::attach_and_seal_in(
        &data,
        &tree,
        None,
        SnapshotTrigger::Manual,
        SealOptions::default(),
    )
    .unwrap();
    let store = open_store(&data).unwrap();
    let snapshot: Snapshot = store.read_struct(&head, ObjectKind::Snapshot).unwrap();
    let many = furrow::tree::find_path(&store, &snapshot.root_tree, b"many")
        .unwrap()
        .unwrap();
    let node: Tree = store
        .read_struct(&many.target.unwrap(), ObjectKind::Tree)
        .unwrap();
    assert!(node.pages.len() > 1, "the fixture is paged");
    // The page levels between the directory node and the leaf holding the name,
    // and how many page nodes the directory has in all.
    let name = b"file-03210-with-a-longer-name.txt".as_slice();
    let (mut depth, mut current) = (0, node.clone());
    while let Some(page) = current
        .pages
        .iter()
        .find(|page| page.first_name.as_slice() <= name && name <= page.last_name.as_slice())
    {
        depth += 1;
        current = store.read_struct(&page.target, ObjectKind::Tree).unwrap();
    }
    let mut page_nodes = 0;
    let mut stack = vec![node];
    while let Some(next) = stack.pop() {
        for page in &next.pages {
            page_nodes += 1;
            stack.push(store.read_struct(&page.target, ObjectKind::Tree).unwrap());
        }
    }
    let all_kinds = kinds(&data, head);
    drop(store);

    let source = Source {
        _temp: tempfile::tempdir().unwrap(),
        data: data.clone(),
        tree: tree.clone(),
        cell: temp.path().join("unused"),
        head,
    };
    let relay: HashMap<_, _> = objects_of(&frame_paths(&export(
        &source,
        LEDGER,
        &temp.path().join("out"),
        FRAME,
    )))
    .into_iter()
    .collect();
    let mut b = reader();
    let path = "many/file-03210-with-a-longer-name.txt";
    let pulled = b.fetch(&relay, head, path, Some("0:"));
    // Snapshot, root, one tree node per page level on the name's way down,
    // the blob, its chunk: never the sibling pages.
    let trees = pulled
        .iter()
        .filter(|rid| all_kinds[*rid] == ObjectKind::Tree)
        .count();
    assert_eq!(
        pulled.len(),
        trees + 3,
        "snapshot, blob, chunk and trees only"
    );
    assert_eq!(trees, 2 + depth, "root, the paged node, one page per level");
    assert!(
        page_nodes > depth + 1,
        "siblings exist that were not fetched"
    );
    assert_eq!(b.read(head, path, "0:").unwrap(), b"3210\n");
    // A full listing of the directory wants every page.
    b.fetch(&relay, head, "many", None);
    assert_eq!(
        b.peek(head, "many").unwrap()["entries"]
            .as_array()
            .unwrap()
            .len(),
        4000
    );
}
