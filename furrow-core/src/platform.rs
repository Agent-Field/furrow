//! Platform boundaries for paths, metadata and durable file replacement.
//!
//! Snapshot names remain byte strings on Unix and UTF-8 with `/` separators
//! on Windows. File contents are never interpreted as text.

use std::fs::{self, File};
use std::io;
use std::path::Path;

#[cfg(windows)]
const REPLACE_DELAYS_MS: [u64; 7] = [10, 20, 40, 80, 160, 320, 640];

#[cfg(unix)]
pub use std::os::unix::{
    ffi::{OsStrExt, OsStringExt},
    fs::{symlink, FileTypeExt, MetadataExt, PermissionsExt},
    process::ExitStatusExt,
};
#[cfg(windows)]
pub use windows::*;
#[cfg(unix)]
pub use xattr;

/// Sync directory entries on Unix. Windows has no supported directory flush;
/// regular files are opened writable so sync_all invokes FlushFileBuffers.
pub fn sync_path(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    #[cfg(windows)]
    {
        if path.is_dir() {
            return Ok(());
        }
        fs::OpenOptions::new().write(true).open(path)?.sync_all()
    }
    #[cfg(unix)]
    File::open(path)?.sync_all()
}

/// Mode changes are absent on Windows, where captured POSIX modes live only
/// in the snapshot. In particular, POSIX read-only bits must not set NTFS ACLs.
pub fn set_mode(path: impl AsRef<Path>, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    return fs::set_permissions(path, fs::Permissions::from_mode(mode));
    #[cfg(windows)]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Editors and antivirus scanners can briefly deny Windows replacement.
/// Retry only sharing/access failures, with a total wait bounded to 1.27s.
pub fn retry_replace<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    #[cfg(windows)]
    for delay in REPLACE_DELAYS_MS {
        match operation() {
            Err(error) if matches!(error.raw_os_error(), Some(5 | 32 | 33)) => {
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }
            result => return result,
        }
    }
    #[cfg(unix)]
    return operation();
    #[cfg(windows)]
    operation().map_err(|error| {
        if matches!(error.raw_os_error(), Some(5 | 32 | 33)) {
            return io::Error::new(
                error.kind(),
                format!("file replacement remained blocked after bounded retries: {error}"),
            );
        }
        error
    })
}

pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    retry_replace(|| fs::rename(from.as_ref(), to.as_ref()))
}

pub fn persist(
    file: tempfile::NamedTempFile,
    path: impl AsRef<Path>,
) -> Result<File, tempfile::PersistError> {
    #[cfg(windows)]
    let mut file = file;
    #[cfg(windows)]
    for delay in REPLACE_DELAYS_MS {
        match file.persist(path.as_ref()) {
            Ok(file) => return Ok(file),
            Err(error) if matches!(error.error.raw_os_error(), Some(5 | 32 | 33)) => {
                file = error.file;
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }
            Err(error) => return Err(error),
        }
    }
    #[cfg(unix)]
    return file.persist(path);
    #[cfg(windows)]
    file.persist(path).map_err(|error| tempfile::PersistError {
        error: replacement_error(error.error),
        file: error.file,
    })
}

/// Convert a local relative path into the platform-independent tree key.
pub fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    return path.as_os_str().as_bytes().to_vec();
    #[cfg(windows)]
    path.as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|byte| if *byte == b'\\' { b'/' } else { *byte })
        .collect()
}

pub trait FileReadAt {
    fn read_exact_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<()>;
}
impl FileReadAt for File {
    #[cfg(unix)]
    fn read_exact_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(self, buffer, offset)
    }
    #[cfg(windows)]
    fn read_exact_at(&self, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
        while !buffer.is_empty() {
            let count = match std::os::windows::fs::FileExt::seek_read(self, buffer, offset) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if count == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            offset += count as u64;
            buffer = &mut buffer[count..];
        }
        Ok(())
    }
}

#[cfg(unix)]
pub fn create_fifo(path: &Path, mode: u32) -> io::Result<()> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: the terminated path is valid for the call.
    if unsafe { libc::mkfifo(path.as_ptr(), mode as libc::mode_t) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    // SAFETY: signal zero checks existence without sending a signal.
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(unix)]
pub fn stop_process(pid: u32) -> io::Result<()> {
    // SAFETY: the PID came from the watcher's pid file.
    if unsafe { libc::kill(pid as i32, libc::SIGTERM) } == 0
        || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::os::windows::fs::MetadataExt as WindowsMetadata;

    pub trait OsStrExt {
        fn as_bytes(&self) -> &[u8];
        fn from_bytes(bytes: &[u8]) -> &Self;
    }
    impl OsStrExt for OsStr {
        fn as_bytes(&self) -> &[u8] {
            self.as_encoded_bytes()
        }
        fn from_bytes(bytes: &[u8]) -> &Self {
            // Unrepresentable imported names are held before reaching the OS.
            OsStr::new(std::str::from_utf8(bytes).unwrap_or("\u{fffd}"))
        }
    }
    pub trait OsStringExt {
        fn from_vec(bytes: Vec<u8>) -> Self;
    }
    impl OsStringExt for OsString {
        fn from_vec(bytes: Vec<u8>) -> Self {
            OsStr::from_bytes(&bytes).to_os_string()
        }
    }
    pub trait MetadataExt {
        fn dev(&self) -> u64;
        fn ino(&self) -> u64;
        fn nlink(&self) -> u64;
        fn mode(&self) -> u32;
        fn mtime(&self) -> i64;
        fn mtime_nsec(&self) -> i64;
        fn ctime(&self) -> i64;
        fn ctime_nsec(&self) -> i64;
        fn blocks(&self) -> u64;
    }
    impl MetadataExt for fs::Metadata {
        // std does not expose stable Windows file IDs. Disable identity-based
        // capture caches and link grouping instead of inventing an identity.
        fn dev(&self) -> u64 {
            0
        }
        fn ino(&self) -> u64 {
            0
        }
        fn nlink(&self) -> u64 {
            1
        }
        fn mode(&self) -> u32 {
            if self.file_type().is_symlink() {
                0o120777
            } else if self.is_dir() {
                0o40755
            } else {
                0o100644
            }
        }
        fn mtime(&self) -> i64 {
            windows_time(self.last_write_time()).0
        }
        fn mtime_nsec(&self) -> i64 {
            windows_time(self.last_write_time()).1
        }
        fn ctime(&self) -> i64 {
            windows_time(self.creation_time()).0
        }
        fn ctime_nsec(&self) -> i64 {
            windows_time(self.creation_time()).1
        }
        fn blocks(&self) -> u64 {
            self.len().div_ceil(512)
        }
    }
    fn windows_time(ticks: u64) -> (i64, i64) {
        let ticks = i128::from(ticks) - 116_444_736_000_000_000;
        (
            ticks.div_euclid(10_000_000) as i64,
            (ticks.rem_euclid(10_000_000) * 100) as i64,
        )
    }
    pub trait FileTypeExt {
        fn is_fifo(&self) -> bool;
        fn is_socket(&self) -> bool;
    }
    impl FileTypeExt for fs::FileType {
        fn is_fifo(&self) -> bool {
            false
        }
        fn is_socket(&self) -> bool {
            false
        }
    }
    pub trait ExitStatusExt {
        fn signal(&self) -> Option<i32>;
    }
    impl ExitStatusExt for std::process::ExitStatus {
        fn signal(&self) -> Option<i32> {
            None
        }
    }

    pub fn symlink(source: impl AsRef<Path>, target: impl AsRef<Path>) -> io::Result<()> {
        let source = source.as_ref();
        let target = target.as_ref();
        let resolved = target.parent().unwrap_or(Path::new(".")).join(source);
        if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(source, target)
        } else {
            std::os::windows::fs::symlink_file(source, target)
        }
    }
    pub fn create_fifo(_: &Path, _: u32) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows cannot create a FIFO",
        ))
    }
    pub fn process_alive(pid: u32) -> bool {
        // SAFETY: the handle is checked and closed exactly once.
        unsafe {
            let handle = OpenProcess(0x1000, 0, pid);
            if handle.is_null() {
                return io::Error::last_os_error().raw_os_error() == Some(5);
            }
            let mut code = 0;
            let ok = GetExitCodeProcess(handle, &mut code) != 0;
            CloseHandle(handle);
            ok && code == 259
        }
    }
    pub fn stop_process(pid: u32) -> io::Result<()> {
        // Watchers are separate console processes; the serving daemon itself
        // uses its console handler to drain requests before closing the store.
        unsafe {
            let handle = OpenProcess(1, 0, pid);
            if handle.is_null() {
                return Ok(());
            }
            let result = TerminateProcess(handle, 0);
            CloseHandle(handle);
            if result != 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
        fn GetExitCodeProcess(handle: *mut std::ffi::c_void, code: *mut u32) -> i32;
        fn TerminateProcess(handle: *mut std::ffi::c_void, code: u32) -> i32;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }

    /// NTFS has no POSIX xattrs. Imported attributes stay in the source head.
    pub mod xattr {
        use super::*;
        pub fn list(_: impl AsRef<Path>) -> io::Result<std::vec::IntoIter<OsString>> {
            Ok(Vec::new().into_iter())
        }
        pub fn get(_: impl AsRef<Path>, _: impl AsRef<OsStr>) -> io::Result<Option<Vec<u8>>> {
            Err(io::ErrorKind::Unsupported.into())
        }
        pub fn set(_: impl AsRef<Path>, _: impl AsRef<OsStr>, _: &[u8]) -> io::Result<()> {
            Err(io::ErrorKind::Unsupported.into())
        }
    }
}

/// A reason an imported entry cannot be represented on Windows. This checks
/// names before any filesystem call, including ancestors of a changed path.
#[cfg(windows)]
pub fn unsupported_name(path: &[u8]) -> Option<String> {
    let Ok(text) = std::str::from_utf8(path) else {
        return Some("name is not UTF-8".into());
    };
    for name in text.split('/') {
        if name.ends_with(['.', ' ']) {
            return Some("Windows refuses a trailing dot or space".into());
        }
        if name.chars().any(|c| c < ' ' || "<>:\"\\|?*".contains(c)) {
            return Some("Windows refuses this character in a name".into());
        }
        let stem = name
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end_matches(' ')
            .to_uppercase();
        if matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|n| {
                matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        }) {
            return Some("Windows reserves this device name".into());
        }
    }
    None
}

/// Probe the actual process token rather than assuming Developer Mode or an
/// elevated token. A refused link is reported before restore mutations begin.
#[cfg(windows)]
pub fn symlink_refusal(root: &Path) -> Option<String> {
    let probe = match tempfile::Builder::new()
        .prefix(".furrow-link-")
        .tempdir_in(root)
    {
        Ok(probe) => probe,
        Err(error) => return Some(format!("cannot test symlink permission: {error}")),
    };
    let target = probe.path().join("target");
    let link = probe.path().join("link");
    if let Err(error) = fs::write(&target, b"") {
        return Some(format!("cannot test symlink permission: {error}"));
    }
    symlink(&target, &link)
        .err()
        .map(|error| format!("cannot create symlink: {error}"))
}

/// NTFS timestamps have 100ns precision; Unix keeps the recorded nanoseconds.
pub fn time_matches(metadata: &fs::Metadata, secs: i64, nanos: u32) -> bool {
    #[cfg(windows)]
    let nanos = nanos / 100 * 100;
    metadata.mtime() == secs && metadata.mtime_nsec().max(0) as u32 == nanos
}

#[cfg(windows)]
fn replacement_error(error: io::Error) -> io::Error {
    if matches!(error.raw_os_error(), Some(5 | 32 | 33)) {
        io::Error::new(
            error.kind(),
            format!("file replacement remained blocked after bounded retries: {error}"),
        )
    } else {
        error
    }
}

/// Merge checks use the platform's native command interpreter.
pub fn shell(command: &str) -> std::process::Command {
    #[cfg(unix)]
    let mut shell = std::process::Command::new("/bin/sh");
    #[cfg(unix)]
    shell.args(["-c", command]);
    #[cfg(windows)]
    let mut shell = std::process::Command::new("cmd.exe");
    #[cfg(windows)]
    shell.args(["/D", "/S", "/C", command]);
    shell
}

/// Include Windows omissions in the caller's existing JSON envelope.
#[cfg(windows)]
pub fn with_held(
    mut answer: serde_json::Value,
    held: &[crate::repository::HeldPath],
) -> anyhow::Result<serde_json::Value> {
    if !held.is_empty() {
        answer["held"] = serde_json::to_value(held)?;
    }
    Ok(answer)
}
