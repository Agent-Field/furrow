use std::io::{self, Read};
use std::path::Path;

pub const SMALL_FILE_LIMIT: usize = 64 * 1024;
pub const MIN_CHUNK: usize = 64 * 1024;
pub const AVG_CHUNK: usize = 256 * 1024;
pub const MAX_CHUNK: usize = 1024 * 1024;
const READ_BUFFER: usize = 64 * 1024;

#[derive(Clone, Copy)]
struct Profile {
    min: usize,
    avg: usize,
    max: usize,
    line_boundaries: bool,
}

const DEFAULT_PROFILE: Profile = Profile {
    min: MIN_CHUNK,
    avg: AVG_CHUNK,
    max: MAX_CHUNK,
    line_boundaries: false,
};
const JOURNAL_PROFILE: Profile = Profile {
    min: 32 * 1024,
    avg: 64 * 1024,
    max: 128 * 1024,
    line_boundaries: true,
};

pub(crate) fn journal_chunks_enabled() -> bool {
    #[cfg(test)]
    if let Some(enabled) = TEST_JOURNAL_CHUNKS.with(|value| value.get()) {
        return enabled;
    }
    std::env::var("FURROW_JOURNAL_CHUNKS").is_ok_and(|value| value == "1")
}

#[cfg(test)]
thread_local! {
    static TEST_JOURNAL_CHUNKS: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_journal_chunks<T>(enabled: bool, run: impl FnOnce() -> T) -> T {
    struct Reset(Option<bool>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_JOURNAL_CHUNKS.with(|value| value.set(self.0));
        }
    }
    let _reset = Reset(TEST_JOURNAL_CHUNKS.with(|value| value.replace(Some(enabled))));
    run()
}

/// Selects journals solely by a case-insensitive `.jsonl` or `.ndjson` filename
/// suffix so capture and estimation agree without inspecting contents or history.
pub(crate) fn is_journal(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.as_encoded_bytes();
        [b".jsonl".as_slice(), b".ndjson".as_slice()]
            .iter()
            .any(|suffix| {
                name.len() >= suffix.len()
                    && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
            })
    })
}

// FastCDC-inspired streaming gear hash. It retains bounded memory and stable
// content-defined boundaries without requiring a full file-sized allocation.
pub struct ChunkStream<R> {
    reader: R,
    buffer: Vec<u8>,
    position: usize,
    filled: usize,
    eof: bool,
    profile: Profile,
}

impl<R: Read> ChunkStream<R> {
    pub fn new(reader: R) -> Self {
        Self::with_profile(reader, DEFAULT_PROFILE)
    }

    /// Smaller journal chunks bound repeated tail uploads while retaining deterministic boundaries.
    pub fn for_path(reader: R, path: &Path) -> Self {
        Self::with_profile(
            reader,
            if journal_chunks_enabled() && is_journal(path) {
                JOURNAL_PROFILE
            } else {
                DEFAULT_PROFILE
            },
        )
    }

    fn with_profile(reader: R, profile: Profile) -> Self {
        Self {
            reader,
            buffer: vec![0; READ_BUFFER],
            position: 0,
            filled: 0,
            eof: false,
            profile,
        }
    }

    pub fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.eof {
            return Ok(None);
        }

        let mut out = Vec::with_capacity(self.profile.avg);
        let mut hash = 0_u64;
        let early_mask = (self.profile.avg as u64 * 2) - 1;
        let late_mask = (self.profile.avg as u64 / 2) - 1;
        while out.len() < self.profile.max {
            if self.position == self.filled {
                self.filled = self.reader.read(&mut self.buffer)?;
                self.position = 0;
                if self.filled == 0 {
                    self.eof = true;
                    break;
                }
            }

            let available = self.filled - self.position;
            let take = available.min(self.profile.max - out.len());
            let start = self.position;
            for offset in 0..take {
                let value = self.buffer[start + offset];
                out.push(value);
                hash = hash.rotate_left(1).wrapping_add(GEAR[value as usize]);

                let len = out.len();
                let boundary = if len < self.profile.min {
                    false
                } else if self.profile.line_boundaries && value == b'\n' {
                    // Closing complete records avoids paying for their bytes on the next append.
                    true
                } else if len < self.profile.avg {
                    hash & early_mask == 0
                } else {
                    hash & late_mask == 0
                };
                if boundary {
                    self.position += offset + 1;
                    return Ok(Some(out));
                }
            }
            self.position += take;
        }

        if out.is_empty() {
            Ok(None)
        } else {
            Ok(Some(out))
        }
    }
}

const fn gear_table() -> [u64; 256] {
    let mut table = [0_u64; 256];
    let mut i = 0;
    let mut state = 0x9e3779b97f4a7c15_u64;
    while i < 256 {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        table[i] = state.wrapping_mul(0x2545f4914f6cdd1d);
        i += 1;
    }
    table
}

const GEAR: [u64; 256] = gear_table();

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn journal_j2_non_journals_keep_036_boundaries_and_ids() {
        check_036_boundaries(&[
            "workspace.bin",
            "a.jsonl/file",
            "transcript.json",
            "events.jsonl.bak",
        ]);
    }

    #[test]
    fn journal_r3_switch_off_keeps_036_journal_boundaries_and_ids() {
        if std::env::var_os("FURROW_JOURNAL_CHUNKS").is_some() {
            // A child tests the actual unset default without mutating the shared environment.
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "chunker::tests::journal_r3_switch_off_keeps_036_journal_boundaries_and_ids"
                ])
                .env_remove("FURROW_JOURNAL_CHUNKS")
                .status()
                .unwrap()
                .success());
            return;
        }
        assert!(!journal_chunks_enabled());
        check_036_boundaries(&["transcript.jsonl", ".cell/worker.NDJSON"]);
    }

    fn check_036_boundaries(paths: &[&str]) {
        let mut state = 0x9e3779b97f4a7c15_u64;
        let input: Vec<u8> = (0..5 * 1024 * 1024 + 17)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect();
        // This independent reference freezes the 0.3.6 gear table and boundary rules.
        let mut gear = [0_u64; 256];
        state = 0x9e3779b97f4a7c15;
        for value in &mut gear {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            *value = state.wrapping_mul(0x2545f4914f6cdd1d);
        }
        let identify =
            |bytes: &[u8]| crate::store::object_id(crate::model::ObjectKind::Chunk, bytes);
        let mut reference = Vec::new();
        let mut start = 0;
        while start < input.len() {
            let mut hash = 0_u64;
            let mut end = start;
            while end < input.len() && end - start < 1024 * 1024 {
                hash = hash.rotate_left(1).wrapping_add(gear[input[end] as usize]);
                end += 1;
                let length = end - start;
                if length >= 64 * 1024
                    && hash & (if length < 256 * 1024 { 524287 } else { 131071 }) == 0
                {
                    break;
                }
            }
            reference.push((end, identify(&input[start..end])));
            start = end;
        }
        assert!(reference.len() > 5);
        for path in paths {
            let mut actual = Vec::new();
            let mut end = 0;
            let mut stream = ChunkStream::for_path(Cursor::new(&input), Path::new(path));
            while let Some(chunk) = stream.next_chunk().unwrap() {
                end += chunk.len();
                actual.push((end, identify(&chunk)));
            }
            assert_eq!(actual, reference, "{path}");
        }
        let mut stream = ChunkStream::new(Cursor::new(&input));
        let mut end = 0;
        for (boundary, id) in reference {
            let chunk = stream.next_chunk().unwrap().unwrap();
            end += chunk.len();
            assert_eq!((end, identify(&chunk)), (boundary, id));
        }
        assert!(stream.next_chunk().unwrap().is_none());
    }

    #[test]
    fn journal_j5_selection_uses_only_filename_suffix() {
        for path in [
            ".jsonl",
            "transcript.JSONL",
            "nested/tasks.jsonl",
            "worker.NdJsOn",
        ] {
            assert!(is_journal(Path::new(path)), "{path}");
        }
        for path in [
            "",
            "jsonl",
            "file.json",
            "file.jsonl.tmp",
            "dir.ndjson/file",
            "file.ndjsonx",
        ] {
            assert!(!is_journal(Path::new(path)), "{path}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert!(is_journal(Path::new(std::ffi::OsStr::from_bytes(
                b"bad-\xff.JSONL"
            ))));
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let mut name = vec![0xd800];
            name.extend(".JSONL".encode_utf16());
            assert!(is_journal(Path::new(&std::ffi::OsString::from_wide(&name))));
        }
    }

    #[test]
    fn chunks_are_bounded_and_reconstruct_input() {
        let input: Vec<u8> = (0..(MAX_CHUNK * 3 + 17)).map(|n| (n % 251) as u8).collect();
        let mut stream = ChunkStream::new(Cursor::new(&input));
        let mut rebuilt = Vec::new();
        let mut count = 0;
        while let Some(chunk) = stream.next_chunk().unwrap() {
            assert!(chunk.len() <= MAX_CHUNK);
            rebuilt.extend_from_slice(&chunk);
            count += 1;
        }
        assert!(count >= 3);
        assert_eq!(rebuilt, input);
    }

    struct CountedReader {
        cursor: Cursor<Vec<u8>>,
        reads: Arc<AtomicUsize>,
    }

    impl Read for CountedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.cursor.read(buffer)
        }
    }

    #[test]
    fn reads_large_inputs_in_blocks() {
        let reads = Arc::new(AtomicUsize::new(0));
        let reader = CountedReader {
            cursor: Cursor::new(vec![3; MAX_CHUNK * 3]),
            reads: Arc::clone(&reads),
        };
        let mut stream = ChunkStream::new(reader);
        let mut length = 0;
        while let Some(chunk) = stream.next_chunk().unwrap() {
            length += chunk.len();
        }
        assert_eq!(length, MAX_CHUNK * 3);
        assert!(reads.load(Ordering::Relaxed) < 100);
    }
}
