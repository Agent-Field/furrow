//! Strict local-only cold storage for Linux and macOS; never a relay object store.
//!
//! Callers must fence every writer before capture and keep that fence through
//! verification and retirement. These commands neither acquire an OS snapshot
//! lock nor stop native watchers. Unknown writers must leave the copy resident.
//! Unsupported ownership, mounts, sparse files, ACLs and filesystem flags are
//! rejected. Restore requires an absent destination and publishes only after
//! validating the complete manifest and syncing restored data and metadata.
#![cfg(unix)]
use crate::chunker::ChunkStream;
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
};
const MAX_CHUNK: usize = 1024 * 1024;
// Generous workspace limits, checked before parsing a database-controlled blob.
const MAX_MANIFEST: usize = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_REFERENCES: usize = 8_000_000;
const MAX_ATTRIBUTES: usize = 64 * 1024 * 1024;
const MAX_PATH: usize = 64 * 1024;

fn parent(path: &Path) -> Result<&Path> {
    let parent = path.parent().context("archive path needs parent")?;
    Ok(if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    })
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    name: Vec<u8>,
    kind: u8,
    mode: u32,
    uid: u32,
    gid: u32,
    secs: i64,
    nanos: i64,
    size: u64,
    links: u64,
    link: Vec<u8>,
    chunks: Vec<String>,
    attrs: Vec<(Vec<u8>, Vec<u8>)>,
    hardlink: Option<Vec<u8>>,
}
#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    entries: Vec<Entry>,
}
#[derive(Serialize)]
pub struct Receipt {
    pub version: u32,
    pub snapshot: String,
    pub files: u64,
    pub logical_bytes: u64,
    pub generated_markers: u64,
}
fn db(store: &Path) -> Result<Connection> {
    ensure!(!store.is_symlink(), "archive store cannot be a symlink");
    fs::create_dir_all(store)?;
    fs::set_permissions(store, fs::Permissions::from_mode(0o700))?;
    let path = store.join("archive-v1.sqlite");
    ensure!(!path.is_symlink(), "archive database cannot be a symlink");
    let conn = Connection::open(&path)?;
    File::open(parent(store)?)?.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    conn.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS chunks(id TEXT PRIMARY KEY, raw_length INTEGER NOT NULL, codec INTEGER NOT NULL, bytes BLOB NOT NULL); CREATE TABLE IF NOT EXISTS snapshots(id TEXT PRIMARY KEY, manifest BLOB NOT NULL);")?;
    Ok(conn)
}
fn safe(root: &Path, name: &[u8]) -> Result<PathBuf> {
    let rel = PathBuf::from(OsString::from_vec(name.to_vec()));
    ensure!(
        rel.components().all(|c| matches!(c, Component::Normal(_))),
        "invalid archive relative path"
    );
    Ok(root.join(rel))
}
fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
fn attrs(path: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut out = Vec::new();
    for name in xattr::list(path)? {
        let raw = name.as_bytes();
        ensure!(
            !raw.starts_with(b"system.posix_acl")
                && raw != b"com.apple.ResourceFork"
                && raw != b"com.apple.decmpfs",
            "unsupported ACL/resource metadata"
        );
        if let Some(value) = xattr::get(path, &name)? {
            out.push((raw.to_vec(), value));
        }
    }
    out.sort();
    Ok(out)
}
#[cfg(target_os = "macos")]
fn platform_check(path: &Path) -> Result<()> {
    use std::ffi::CString;
    let p = CString::new(path.as_os_str().as_bytes())?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::lstat(p.as_ptr(), &mut st) } == 0,
        "cannot inspect archive flags"
    );
    ensure!(st.st_flags == 0, "unsupported filesystem flags");
    unsafe extern "C" {
        fn acl_get_link_np(path: *const libc::c_char, kind: libc::c_int) -> *mut libc::c_void;
        fn acl_get_entry(
            acl: *mut libc::c_void,
            id: libc::c_int,
            entry: *mut *mut libc::c_void,
        ) -> libc::c_int;
        fn acl_free(obj: *mut libc::c_void) -> libc::c_int;
    }
    let acl = unsafe { acl_get_link_np(p.as_ptr(), 0x100) };
    if !acl.is_null() {
        let mut entry = std::ptr::null_mut();
        let status = unsafe { acl_get_entry(acl, 0, &mut entry) };
        unsafe { acl_free(acl) };
        ensure!(
            status == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT),
            "unsupported Darwin ACL"
        );
    } else {
        let e = std::io::Error::last_os_error();
        ensure!(
            matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTSUP)),
            "cannot inspect Darwin ACL"
        );
    }
    Ok(())
}
#[cfg(target_os = "linux")]
fn platform_check(path: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    let m = fs::symlink_metadata(path)?;
    if m.is_file() || m.is_dir() {
        let f = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let mut flags: libc::c_long = 0;
        let r = unsafe { libc::ioctl(f.as_raw_fd(), 0x80086601u64, &mut flags) };
        if r == 0 {
            // Linux fs.h: extents and directory hash indexes describe storage
            // layout, not metadata that must survive restoring on another FS.
            let structural_flags = 0x80000 | if m.is_dir() { 0x1000 } else { 0 };
            ensure!(
                flags & !structural_flags == 0,
                "unsupported filesystem flags"
            );
        } else {
            let e = std::io::Error::last_os_error();
            ensure!(
                matches!(
                    e.raw_os_error(),
                    Some(libc::ENOTTY) | Some(libc::EOPNOTSUPP)
                ),
                "cannot inspect filesystem flags"
            );
        }
    }
    Ok(())
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_check(_path: &Path) -> Result<()> {
    bail!("unsupported exact archive platform")
}
fn add_chunk(conn: &Connection, bytes: &[u8]) -> Result<String> {
    let id = digest(bytes);
    if conn
        .query_row("SELECT 1 FROM chunks WHERE id=?", [&id], |_| Ok(()))
        .is_ok()
    {
        let existing = read_chunk(conn, &id)?;
        ensure!(existing == bytes, "archive chunk collision");
        return Ok(id);
    }
    let packed = zstd::bulk::compress(bytes, 3)?;
    let (codec, payload) = if packed.len() < bytes.len() {
        (1, packed.as_slice())
    } else {
        (0, bytes)
    };
    conn.execute(
        "INSERT INTO chunks VALUES(?1,?2,?3,?4)",
        params![id, bytes.len() as i64, codec, payload],
    )?;
    Ok(id)
}
fn read_chunk(conn: &Connection, id: &str) -> Result<Vec<u8>> {
    let encoded: i64 =
        conn.query_row("SELECT length(bytes) FROM chunks WHERE id=?", [id], |r| {
            r.get(0)
        })?;
    ensure!(
        encoded > 0 && encoded <= MAX_CHUNK as i64,
        "invalid encoded archive length"
    );
    let (len, codec, bytes): (i64, i64, Vec<u8>) = conn.query_row(
        "SELECT raw_length,codec,bytes FROM chunks WHERE id=?",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    ensure!(
        len > 0 && len <= MAX_CHUNK as i64,
        "invalid archive chunk length"
    );
    let raw = match codec {
        0 => bytes,
        1 => zstd::bulk::decompress(&bytes, len as usize)?,
        _ => bail!("unsupported archive codec"),
    };
    ensure!(
        raw.len() == len as usize && digest(&raw) == id,
        "corrupt archive chunk"
    );
    Ok(raw)
}
fn scan(root: &Path, conn: &Connection, save: bool) -> Result<Manifest> {
    let root = fs::canonicalize(root)?;
    ensure!(root.is_dir(), "archive workspace must be a directory");
    let mut entries = Vec::new();
    let mut links = BTreeMap::new();
    walk(
        &root,
        &root,
        fs::symlink_metadata(&root)?.dev(),
        conn,
        save,
        &mut links,
        &mut entries,
    )?;
    Ok(Manifest {
        version: 1,
        entries,
    })
}
fn walk(
    root: &Path,
    path: &Path,
    root_device: u64,
    conn: &Connection,
    save: bool,
    links: &mut BTreeMap<(u64, u64), Vec<u8>>,
    out: &mut Vec<Entry>,
) -> Result<()> {
    let before = fs::symlink_metadata(path)?;
    ensure!(
        before.dev() == root_device,
        "unsupported nested filesystem mount"
    );
    platform_check(path)?;
    ensure!(
        before.uid() == unsafe { libc::geteuid() } && before.gid() == unsafe { libc::getegid() },
        "unsupported foreign owner or group"
    );
    let name = path.strip_prefix(root)?.as_os_str().as_bytes().to_vec();
    let kind = if before.is_dir() {
        0
    } else if before.is_file() {
        1
    } else if before.file_type().is_symlink() {
        2
    } else {
        bail!("unsupported special filesystem entry")
    };
    if kind == 1 {
        ensure!(
            before.size() == 0 || before.blocks() * 512 >= before.size(),
            "unsupported sparse file"
        );
    }
    let mut e = Entry {
        name: name.clone(),
        kind,
        mode: before.mode() & 0o7777,
        uid: before.uid(),
        gid: before.gid(),
        secs: before.mtime(),
        nanos: before.mtime_nsec(),
        size: if kind == 1 { before.size() } else { 0 },
        links: if kind == 1 { before.nlink() } else { 0 },
        link: Vec::new(),
        chunks: Vec::new(),
        attrs: attrs(path)?,
        hardlink: None,
    };
    if kind == 2 {
        e.link = fs::read_link(path)?.as_os_str().as_bytes().to_vec();
    }
    if kind == 1 {
        if before.nlink() > 1 {
            let key = (before.dev(), before.ino());
            if let Some(first) = links.get(&key) {
                e.hardlink = Some(first.clone())
            } else {
                links.insert(key, name);
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let opened = file.metadata()?;
        ensure!(
            opened.ino() == before.ino() && opened.dev() == before.dev(),
            "workspace changed during capture"
        );
        let mut stream = ChunkStream::new(file);
        while let Some(bytes) = stream.next_chunk()? {
            let id = if save {
                add_chunk(conn, &bytes)?
            } else {
                digest(&bytes)
            };
            e.chunks.push(id);
        }
    }
    out.push(e);
    if kind == 0 {
        let mut children = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(|d| d.file_name());
        for child in children {
            walk(root, &child.path(), root_device, conn, save, links, out)?;
        }
    }
    let after = fs::symlink_metadata(path)?;
    ensure!(
        before.ino() == after.ino()
            && before.dev() == after.dev()
            && before.size() == after.size()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec(),
        "workspace changed during capture"
    );
    Ok(())
}
// Check the entire hierarchy before any restore path is created. Every parent
// must already be a directory in this manifest; symlinks can never be ancestors.
fn validate_manifest(m: &Manifest) -> Result<()> {
    ensure!(m.version == 1, "unsupported archive version");
    ensure!(
        !m.entries.is_empty() && m.entries.len() <= MAX_ENTRIES,
        "archive entry budget exceeded"
    );
    let mut seen: BTreeMap<&[u8], u8> = BTreeMap::new();
    let mut references = 0usize;
    let mut attributes = 0usize;
    let mut logical_bytes = 0u64;
    for (index, e) in m.entries.iter().enumerate() {
        ensure!(
            e.name.len() <= MAX_PATH && e.link.len() <= MAX_PATH,
            "archive path budget exceeded"
        );
        ensure!(
            e.kind <= 2 && e.nanos < 1_000_000_000 && e.mode & !0o7777 == 0,
            "invalid archive entry metadata"
        );
        if index == 0 {
            ensure!(e.name.is_empty() && e.kind == 0, "invalid archive root");
        } else {
            ensure!(!e.name.is_empty(), "duplicate archive root");
            let path = safe(Path::new(""), &e.name)?;
            let parent = path.parent().context("invalid archive parent")?;
            ensure!(
                seen.get(parent.as_os_str().as_bytes()) == Some(&0),
                "invalid archive ancestor"
            );
        }
        if let Some(target) = &e.hardlink {
            ensure!(
                e.kind == 1 && seen.get(target.as_slice()) == Some(&1),
                "invalid archive hardlink target"
            );
        }
        ensure!(
            seen.insert(e.name.as_slice(), e.kind).is_none(),
            "duplicate archive path"
        );
        logical_bytes = logical_bytes
            .checked_add(e.size)
            .context("archive logical size budget exceeded")?;
        references = references
            .checked_add(e.chunks.len())
            .context("archive reference budget exceeded")?;
        ensure!(
            references <= MAX_REFERENCES,
            "archive reference budget exceeded"
        );
        ensure!(
            e.chunks
                .iter()
                .all(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())),
            "invalid archive chunk reference"
        );
        for (name, value) in &e.attrs {
            attributes = attributes
                .checked_add(name.len())
                .and_then(|n| n.checked_add(value.len()))
                .context("archive attribute budget exceeded")?;
            ensure!(
                attributes <= MAX_ATTRIBUTES,
                "archive attribute budget exceeded"
            );
        }
    }
    Ok(())
}
fn load(conn: &Connection, id: &str) -> Result<Manifest> {
    // CASE avoids materializing an oversized SQLite blob in Rust at all.
    let bytes: Option<Vec<u8>> = conn.query_row(
        "SELECT CASE WHEN length(manifest)<=?2 THEN manifest ELSE NULL END FROM snapshots WHERE id=?1",
        params![id, MAX_MANIFEST as u64], |r| r.get(0),
    )?;
    let bytes = bytes.context("archive manifest exceeds byte budget")?;
    ensure!(digest(&bytes) == id, "corrupt archive manifest");
    let m: Manifest = serde_json::from_slice(&bytes)?;
    validate_manifest(&m)?;
    Ok(m)
}
fn receipt(id: String, m: &Manifest) -> Receipt {
    Receipt {
        version: 1,
        snapshot: id,
        files: m.entries.iter().filter(|e| e.kind == 1).count() as u64,
        logical_bytes: m.entries.iter().map(|e| e.size).sum(),
        generated_markers: 0,
    }
}
pub fn capture(store: &Path, workspace: &Path) -> Result<Receipt> {
    ensure!(
        fs::symlink_metadata(workspace)?.is_dir(),
        "workspace root must be a real directory"
    );
    let workspace = fs::canonicalize(workspace)?;
    let conn = db(store)?;
    ensure!(
        !fs::canonicalize(store)?.starts_with(&workspace),
        "archive store must be outside workspace"
    );
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        let m = scan(&workspace, &conn, true)?;
        let mut groups = BTreeMap::new();
        for e in m.entries.iter().filter(|e| e.kind == 1) {
            *groups
                .entry(e.hardlink.as_ref().unwrap_or(&e.name))
                .or_insert(0u64) += 1;
        }
        for e in m.entries.iter().filter(|e| e.kind == 1) {
            ensure!(
                groups[e.hardlink.as_ref().unwrap_or(&e.name)] == e.links,
                "unsupported hardlink outside workspace"
            );
        }
        let check = scan(&workspace, &conn, false)?;
        ensure!(
            m.entries == check.entries,
            "workspace changed during capture"
        );
        validate_manifest(&m)?;
        let bytes = serde_json::to_vec(&m)?;
        ensure!(
            bytes.len() <= MAX_MANIFEST,
            "archive manifest exceeds byte budget"
        );
        let id = digest(&bytes);
        conn.execute(
            "INSERT OR IGNORE INTO snapshots VALUES(?1,?2)",
            params![id, bytes],
        )?;
        Ok(receipt(id, &m))
    })();
    match result {
        Ok(r) => {
            conn.execute_batch("COMMIT")?;
            File::open(store)?.sync_all()?;
            Ok(r)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}
// macOS adds this origin marker while materializing files. Only an extra key
// is allowed: every attribute actually captured remains byte-for-byte required.
#[cfg(target_os = "macos")]
fn generated_marker(expected: &Entry, actual: &mut Entry) -> u64 {
    const KEY: &[u8] = b"com.apple.provenance";
    if !expected.attrs.iter().any(|(k, _)| k == KEY) {
        let n = actual.attrs.len();
        actual.attrs.retain(|(k, _)| k != KEY);
        return (n - actual.attrs.len()) as u64;
    }
    0
}
#[cfg(not(target_os = "macos"))]
fn generated_marker(_expected: &Entry, _actual: &mut Entry) -> u64 {
    0
}
fn normalize_markers(expected: &[Entry], actual: &mut [Entry]) -> u64 {
    if expected.len() != actual.len() {
        return 0;
    }
    expected
        .iter()
        .zip(actual)
        .filter_map(|(a, b)| {
            if a.name == b.name {
                Some(generated_marker(a, b))
            } else {
                None
            }
        })
        .sum()
}
pub fn verify(store: &Path, id: &str, workspace: &Path) -> Result<Receipt> {
    let conn = db(store)?;
    let m = load(&conn, id)?;
    for e in &m.entries {
        for chunk in &e.chunks {
            read_chunk(&conn, chunk)?;
        }
    }
    let mut actual = scan(workspace, &conn, false)?;
    let markers = normalize_markers(&m.entries, &mut actual.entries);
    ensure!(
        m.entries == actual.entries,
        "archive differs from workspace"
    );
    let mut r = receipt(id.to_string(), &m);
    r.generated_markers = markers;
    Ok(r)
}
fn difference(expected: &[Entry], actual: &[Entry]) -> String {
    if expected.len() != actual.len() {
        return "entry count".into();
    }
    for (a, b) in expected.iter().zip(actual) {
        if a != b {
            let mut fields = Vec::new();
            if a.name != b.name {
                fields.push("name");
            }
            if a.kind != b.kind {
                fields.push("kind");
            }
            if a.mode != b.mode {
                fields.push("mode");
            }
            if a.uid != b.uid || a.gid != b.gid {
                fields.push("ownership");
            }
            if a.secs != b.secs || a.nanos != b.nanos {
                fields.push("mtime");
            }
            if a.attrs != b.attrs {
                fields.push("xattrs");
            }
            if a.link != b.link {
                fields.push("symlink target");
            }
            if a.chunks != b.chunks || a.size != b.size {
                fields.push("content");
            }
            if a.links != b.links || a.hardlink != b.hardlink {
                fields.push("hardlinks");
            }
            return format!("kind {} fields {}", a.kind, fields.join(", "));
        }
    }
    "unknown".into()
}
struct Cleanup<'a>(&'a Path);
impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        fn prepare(p: &Path) {
            if let Ok(m) = fs::symlink_metadata(p) {
                if m.is_dir() {
                    let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
                    if let Ok(children) = fs::read_dir(p) {
                        for child in children.flatten() {
                            prepare(&child.path());
                        }
                    }
                }
            }
        }
        prepare(self.0);
    }
}
pub fn restore(store: &Path, id: &str, destination: &Path) -> Result<Receipt> {
    ensure!(
        !destination.exists() && fs::symlink_metadata(destination).is_err(),
        "archive destination must not exist"
    );
    let conn = db(store)?;
    let m = load(&conn, id)?;
    let parent = parent(destination)?;
    let temp = tempfile::Builder::new()
        .prefix(".furrow-archive-")
        .tempdir_in(parent)?;
    let root = temp.path();
    let _cleanup = Cleanup(root);
    for e in &m.entries {
        let p = if e.name.is_empty() {
            root.to_path_buf()
        } else {
            safe(root, &e.name)?
        };
        if e.name.is_empty() {
            ensure!(e.kind == 0, "invalid archive root");
            continue;
        }
        ensure!(
            p.parent().is_some_and(|v| v.is_dir() && !v.is_symlink()),
            "invalid archive parent"
        );
        match e.kind {
            0 => fs::create_dir(&p)?,
            1 => {
                if let Some(target) = &e.hardlink {
                    let original = safe(root, target)?;
                    ensure!(
                        original.is_file() && !original.is_symlink(),
                        "invalid hardlink target"
                    );
                    fs::hard_link(original, &p)?;
                } else {
                    let mut f = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&p)?;
                    let mut length = 0;
                    for id in &e.chunks {
                        let bytes = read_chunk(&conn, id)?;
                        length += bytes.len() as u64;
                        f.write_all(&bytes)?;
                    }
                    ensure!(length == e.size, "invalid archive file length");
                    f.sync_all()?;
                }
            }
            2 => std::os::unix::fs::symlink(OsString::from_vec(e.link.clone()), &p)?,
            _ => bail!("unsupported archive entry"),
        }
    }
    for e in m.entries.iter().rev() {
        let p = if e.name.is_empty() {
            root.to_path_buf()
        } else {
            safe(root, &e.name)?
        };
        // Newly created paths can inherit attributes absent from the archive.
        // An exact restore removes those too, or fails without publishing.
        for name in xattr::list(&p)? {
            if !e
                .attrs
                .iter()
                .any(|(expected, _)| expected.as_slice() == name.as_bytes())
            {
                xattr::remove(&p, &name)?;
            }
        }
        for (name, value) in &e.attrs {
            xattr::set(&p, OsString::from_vec(name.clone()), value)?;
        }
    }
    for e in m.entries.iter().rev() {
        let p = if e.name.is_empty() {
            root.to_path_buf()
        } else {
            safe(root, &e.name)?
        };
        if e.kind != 2 {
            fs::set_permissions(&p, fs::Permissions::from_mode(e.mode))?;
        } else {
            #[cfg(target_os = "macos")]
            {
                unsafe extern "C" {
                    fn lchmod(path: *const libc::c_char, mode: libc::mode_t) -> libc::c_int;
                }
                let name = std::ffi::CString::new(p.as_os_str().as_bytes())?;
                ensure!(
                    unsafe { lchmod(name.as_ptr(), e.mode as libc::mode_t) } == 0,
                    "cannot restore symlink permissions"
                );
            }
        }
        let time = filetime::FileTime::from_unix_time(e.secs, e.nanos as u32);
        filetime::set_symlink_file_times(&p, time, time)?;
    }
    let mut actual = scan(root, &conn, false)?;
    let markers = normalize_markers(&m.entries, &mut actual.entries);
    ensure!(
        actual.entries == m.entries,
        "restored archive metadata or contents differ: {}",
        difference(&m.entries, &actual.entries)
    );
    // Flush again after xattrs, permissions and timestamps. Sync children before
    // their containing directories, including the temporary root, before publish.
    for e in m.entries.iter().rev().filter(|e| e.kind != 2) {
        let path = if e.name.is_empty() {
            root.to_path_buf()
        } else {
            safe(root, &e.name)?
        };
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?
            .sync_all()?;
    }
    // Refuse a destination created after the initial existence check.
    let from = std::ffi::CString::new(root.as_os_str().as_bytes())?;
    let to = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
    #[cfg(target_os = "linux")]
    let renamed = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let renamed = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let renamed = -1;
    ensure!(
        renamed == 0,
        "cannot atomically publish archive restore: {}",
        std::io::Error::last_os_error()
    );
    File::open(parent)?.sync_all()?;
    let mut r = receipt(id.to_string(), &m);
    r.generated_markers = markers;
    Ok(r)
}

/// Proves an interrupted deletion contains only unchanged captured entries.
pub fn verify_remainder(store: &Path, id: &str, workspace: &Path) -> Result<Receipt> {
    let conn = db(store)?;
    let m = load(&conn, id)?;
    for e in &m.entries {
        for chunk in &e.chunks {
            read_chunk(&conn, chunk)?;
        }
    }
    let actual = scan(workspace, &conn, false)?;
    let expected: BTreeMap<_, _> = m.entries.iter().map(|e| (e.name.clone(), e)).collect();
    let mut mapping = BTreeMap::new();
    let mut reverse = BTreeMap::new();
    let mut group_counts = BTreeMap::new();
    let mut markers = 0;
    for a in &actual.entries {
        let e = expected
            .get(&a.name)
            .context("retirement remainder contains a new path")?;
        let mut a = a.clone();
        let mut e = (*e).clone();
        markers += generated_marker(&e, &mut a);
        if a.kind == 0 && e.kind == 0 {
            ensure!(
                a.mode == e.mode || a.mode == (e.mode | 0o700),
                "retirement directory permissions changed"
            );
            a.mode = e.mode;
            a.secs = e.secs;
            a.nanos = e.nanos;
        }
        if a.kind == 1 && e.kind == 1 {
            let old = e.hardlink.as_ref().unwrap_or(&e.name).clone();
            let new = a.hardlink.as_ref().unwrap_or(&a.name).clone();
            if let Some(seen) = mapping.insert(old.clone(), new.clone()) {
                ensure!(seen == new, "retirement hardlink group split");
            }
            if let Some(seen) = reverse.insert(new.clone(), old.clone()) {
                ensure!(seen == old, "retirement hardlink groups merged");
            }
            *group_counts.entry(new).or_insert(0u64) += 1;
            a.links = 0;
            e.links = 0;
            a.hardlink = None;
            e.hardlink = None;
        }
        ensure!(
            a == e,
            "retirement remainder changed: {}",
            difference(&[e], &[a])
        );
    }
    for a in actual.entries.iter().filter(|e| e.kind == 1) {
        let key = a.hardlink.as_ref().unwrap_or(&a.name);
        ensure!(
            a.links == group_counts[key],
            "retirement remainder has an external hardlink"
        );
    }
    let mut r = receipt(id.to_string(), &m);
    r.generated_markers = markers;
    Ok(r)
}
