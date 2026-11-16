# Reading one file of a relayed snapshot

`want --path`, `peek` and `read` let a computer read **one file** (a chat's
`.cell/transcript.jsonl`) out of a snapshot held on a relay without
downloading the rest of the snapshot. The code is
`furrow-core/src/exchange/path.rs`. The command line (`furrow-cli/src/exchange.rs`)
and the daemon (`furrow-daemon/src/verbs.rs`) both call the same functions in
`furrow-core/src/exchange/ops.rs`.

## Why this exists, and why it must not become a full fetch

A chat lives in a cell: a project folder with the chat's `.cell/` directory
composed in as the tree's `.cell` entry. The whole-snapshot `want` walks every
tree and blob, so before these verbs the only way to read a chat held by
another computer was a take, which downloads the whole project folder.
**Reading a remote chat must not cost its project folder.** Three properties
keep it cheap. Do not "simplify" them away:

1. **The walk is path-scoped.** A path want walks only the snapshot, one tree
   node (or tree page) per path component, and the file's blob index. In a
   paged directory only the page whose name range covers the component is
   read. Opening a chat costs a few KB of index objects.
2. **Ranges ride on content-defined chunks.** With `--range S:E` only the
   chunks overlapping `[S,E)` are wanted. Chunk boundaries depend on content,
   not offset (`furrow-core/src/chunker.rs`). Appending a turn therefore leaves
   every earlier chunk byte-identical. Re-reading the tail after a new turn
   downloads the changed index objects and the new tail chunk(s) only.
3. **The objects land in the normal store.** Path-scoped objects are imported by
   the ordinary strict `import` under their ordinary ids. A later full `want` /
   `import` / `materialize` (a take) sees them as held and fetches only the
   rest. A later take never pays twice. The relay and the encryption are
   untouched: these verbs only choose which existing objects to ask for.

## The verbs

Keys come from `FURROW_CELL_KEY` / `FURROW_DEDUP_SECRET`, as they do for `want`.
The store is selected the way the other exchange verbs select it
(`FURROW_DATA_DIR`, `--timeline`). Each verb can be spelled with or without the
`exchange` word.

```sh
furrow --json want --head H --path .cell/transcript.jsonl                 # spine + blob index
furrow --json want --head H --path .cell/transcript.jsonl --range S:E     # + overlapping chunks
furrow --json peek --head H --path .cell/transcript.jsonl                 # held objects only
furrow --json read --head H --path .cell/transcript.jsonl --range S:E --out FILE
furrow --json capabilities                                                # {"version", "verbs": [...]}
```

- `want` answers `{"want": [rid, ...]}`, at most `WANT_LIMIT`, nearest the
  snapshot first. An object that is not held cannot be opened, so the caller
  loops want → fetch → import until the answer is empty. This is the same
  shape and the same loop as the whole-snapshot `want`.
- `peek` never touches the network.
  - A file answers `{"kind":"file","len":N,"chunks":[{"start","end","held"}]}`.
  - A directory answers `{"kind":"dir","entries":[{"name","kind","len"}]}`,
    where `kind` is `file`, `dir`, `symlink` or `other`.
- `read` writes exactly bytes `[S,E)` to `FILE` and answers `{"bytes": n}`.
  It writes through a temporary file and renames, so a reader never sees half
  a range. It restores nothing else: no tree, no workspace, no head move.
- Range rules:
  - `E` past the end of the file is clamped to the file's length.
  - `S:` means to the end of the file.
  - `S == E` names nothing: `want` answers empty and `read` writes 0 bytes.
  - `S` past the end of the file is `bad_range`.
- Over the daemon socket, the same verbs are `want` (with optional `path`,
  `start`, `end`), `peek` (`head`, `path`), `read` (`head`, `path`, `start`,
  `end`, `out`) and `capabilities`.

## Refusals

A refusal is **one JSON object**. On the command line it is the whole of
stderr (exit status 1). Over the socket it is the whole `err` string:

```json
{"code":"incomplete","path":".cell/transcript.jsonl","kind":"chunk","want":"<rid>","message":"..."}
```

| `code` | Meaning |
| --- | --- |
| `incomplete` | An index object or chunk is not held. `want` (present when keys were given) is its remote id. |
| `not_found` | The snapshot has no such path. |
| `not_a_file` | A range was asked of a directory. |
| `bad_range` | The range is not `S:E`, has `E < S`, or starts past the end. |

## `.cell/` paths

codeaf seals a cell with the `.cell` directory composed in as an overlay
(`furrow-core/src/overlay.rs`). The overlay is sealed as an ordinary
`.cell` directory entry at the top of the snapshot's tree. So
`--path .cell/transcript.jsonl` resolves through the ordinary tree walk, with
no special case. `materialize --cell-dir` only changes where that entry is
restored on disk.

## A partial store

A store holding a path-scoped subset of a snapshot is a normal store:
- `verify` passes.
- `gc` does not error. It may collect the objects, because a read roots
  nothing.
- A later path want or full take fetches whatever is missing.

Tests: `furrow-core/tests/path_read.rs`,
`furrow-cli/tests/exchange.rs::a_transcript_reads_through_the_binary_without_its_project`,
and `furrow-daemon/tests/stage1_verbs.rs::path_want_peek_and_read_over_socket`.
