//! Directory enumeration supplies file identity without opening individual entries.

use super::*;
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::{
    ffi::{OsStrExt, OsStringExt},
    fs::OpenOptionsExt,
    io::AsRawHandle,
};
use windows_sys::Win32::Storage::FileSystem::*;

/// Query an already-open content or directory handle; this never opens a file.
pub fn handle_identity(file: &File) -> io::Result<FileIdentity> {
    // SAFETY: the writable structure has the requested size and the handle stays live.
    unsafe {
        let mut info: FILE_ID_INFO = std::mem::zeroed();
        if GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of_val(&info) as u32,
        ) != 0
        {
            return Ok((
                info.VolumeSerialNumber,
                u128::from_le_bytes(info.FileId.Identifier),
            ));
        }
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(1 | 50 | 87 | 124)) {
            return Err(error);
        }
        let mut legacy: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        if GetFileInformationByHandle(file.as_raw_handle(), &mut legacy) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((
            u64::from(legacy.dwVolumeSerialNumber),
            u128::from((u64::from(legacy.nFileIndexHigh) << 32) | u64::from(legacy.nFileIndexLow)),
        ))
    }
}

/// Both enumeration classes return metadata and file IDs in variable-length records.
/// The fallback preserves the full 64-bit ID supplied by older file systems.
pub fn directory_identities(path: &Path) -> io::Result<HashMap<OsString, FileIdentity>> {
    let directory = fs::OpenOptions::new()
        .read(true)
        .share_mode(7)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    #[cfg(test)]
    DIRECTORY_OPENS.with(|count| count.set(count.get() + 1));
    let volume = handle_identity(&directory)?.0;
    enumerate(&directory, volume, false).or_else(|error| {
        if matches!(error.raw_os_error(), Some(1 | 50 | 87 | 124)) {
            enumerate(&directory, volume, true)
        } else {
            Err(error)
        }
    })
}

fn enumerate(
    directory: &File,
    volume: u64,
    legacy: bool,
) -> io::Result<HashMap<OsString, FileIdentity>> {
    let mut entries = HashMap::new();
    // An aligned allocation lets Windows fill either record type, including long names.
    let mut buffer = vec![0_u64; 8192];
    let length = buffer.len() * 8;
    loop {
        let class = if legacy {
            FileIdBothDirectoryInfo
        } else {
            FileIdExtdDirectoryInfo
        };
        // SAFETY: the buffer is writable, aligned, and its byte length is correct.
        if unsafe {
            GetFileInformationByHandleEx(
                directory.as_raw_handle(),
                class,
                buffer.as_mut_ptr().cast(),
                length as u32,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(18) {
                Ok(entries)
            } else {
                Err(error)
            };
        }
        let mut offset = 0;
        loop {
            let base = buffer.as_ptr().cast::<u8>();
            let (header, name_offset) = if legacy {
                (
                    std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>(),
                    std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName),
                )
            } else {
                (
                    std::mem::size_of::<FILE_ID_EXTD_DIR_INFO>(),
                    std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName),
                )
            };
            if offset + header > length {
                return Err(io::ErrorKind::InvalidData.into());
            }
            // SAFETY: the fixed header is in bounds. Windows supplies aligned offsets;
            // read_unaligned also handles a file system returning a lesser alignment.
            let (next, name_len, id) = unsafe {
                if legacy {
                    let entry =
                        std::ptr::read_unaligned(base.add(offset).cast::<FILE_ID_BOTH_DIR_INFO>());
                    (
                        entry.NextEntryOffset as usize,
                        entry.FileNameLength as usize,
                        entry.FileId as u64 as u128,
                    )
                } else {
                    let entry =
                        std::ptr::read_unaligned(base.add(offset).cast::<FILE_ID_EXTD_DIR_INFO>());
                    (
                        entry.NextEntryOffset as usize,
                        entry.FileNameLength as usize,
                        u128::from_le_bytes(entry.FileId.Identifier),
                    )
                }
            };
            if name_len % 2 != 0
                || offset + name_offset + name_len > length
                || (next != 0 && (next < name_offset + name_len || offset + next >= length))
            {
                return Err(io::ErrorKind::InvalidData.into());
            }
            // SAFETY: the UTF-16 name is aligned and its entire range was checked above.
            let name = unsafe {
                std::slice::from_raw_parts(
                    base.add(offset + name_offset).cast::<u16>(),
                    name_len / 2,
                )
            };
            let name = OsString::from_wide(name);
            if name != "." && name != ".." && id != 0 {
                entries.insert(name, (volume, id));
            }
            if next == 0 {
                break;
            }
            offset += next;
        }
    }
}

/// Git may retain a name's previous case after an NTFS case-only rename.
/// Preserve the directory's identity while capturing under the caller's logical spelling.
pub fn entry_identity(
    entries: &HashMap<OsString, FileIdentity>,
    name: &std::ffi::OsStr,
) -> Option<FileIdentity> {
    if let Some(identity) = entries.get(name) {
        return Some(*identity);
    }
    let wanted: Vec<u16> = name.encode_wide().collect();
    entries.iter().find_map(|(listed, identity)| {
        let listed: Vec<u16> = listed.encode_wide().collect();
        // SAFETY: both buffers are live for the call and their explicit lengths exclude terminators.
        let equal = unsafe {
            windows_sys::Win32::Globalization::CompareStringOrdinal(
                listed.as_ptr(),
                listed.len() as i32,
                wanted.as_ptr(),
                wanted.len() as i32,
                1,
            )
        } == windows_sys::Win32::Globalization::CSTR_EQUAL;
        equal.then_some(*identity)
    })
}

/// A hinted path needs its link count after an index is reconstructed from
/// portable trees, which deliberately carry no local inode numbers. Full walks
/// discover aliases from enumeration identities and never call this helper.
pub fn path_link_count(path: &Path) -> io::Result<u32> {
    let file = fs::OpenOptions::new().read(true).share_mode(7).open(path)?;
    // SAFETY: the structure is writable and the file handle remains live.
    unsafe {
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        if GetFileInformationByHandle(file.as_raw_handle(), &mut info) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.nNumberOfLinks)
    }
}

/// Delta captures enumerate the parent once rather than opening the changed entry for identity.
pub fn path_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    Ok(entry_identity(
        &directory_identities(parent)?,
        path.file_name().ok_or(io::ErrorKind::InvalidInput)?,
    ))
}

#[cfg(test)]
thread_local! {
    pub(crate) static DIRECTORY_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m2_extended_and_legacy_listings_agree_on_ntfs_links() {
        // M2: the fallback class retains NTFS file IDs and distinguishes unrelated equal-byte files.
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("one"), b"bytes").unwrap();
        fs::hard_link(temp.path().join("one"), temp.path().join("two")).unwrap();
        fs::write(temp.path().join("other"), b"bytes").unwrap();
        let directory = fs::OpenOptions::new()
            .read(true)
            .share_mode(7)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(temp.path())
            .unwrap();
        let volume = handle_identity(&directory).unwrap().0;
        let legacy = enumerate(&directory, volume, true).unwrap();
        let extended = directory_identities(temp.path()).unwrap();
        assert_eq!(legacy, extended);
        assert_eq!(
            legacy[&OsString::from("one")],
            legacy[&OsString::from("two")]
        );
        assert_ne!(
            legacy[&OsString::from("one")],
            legacy[&OsString::from("other")]
        );
    }

    #[test]
    fn m2_case_only_spellings_keep_the_directory_file_identity() {
        // M2/R1: Git's logical spelling and NTFS's actual case must resolve to the same file ID.
        let temp = tempfile::tempdir().unwrap();
        let actual = temp.path().join("ÄLIAS");
        fs::write(&actual, b"bytes").unwrap();
        let entries = directory_identities(temp.path()).unwrap();
        let expected = entry_identity(&entries, std::ffi::OsStr::new("ÄLIAS")).unwrap();
        assert_eq!(
            entry_identity(&entries, std::ffi::OsStr::new("älias")),
            Some(expected)
        );
        assert_eq!(
            path_identity(&temp.path().join("älias")).unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn m2_full_128_bit_file_ids_round_trip_through_sqlite() {
        // M2/R1: IDs differing only in the high bits cannot alias in the stat cache or link index.
        let temp = tempfile::tempdir().unwrap();
        let index = crate::path_index::PathIndex::open(&temp.path().join("paths")).unwrap();
        let id = 0x123456789abcdef0fedcba9876543210_u128;
        index.note_link(b"a", Some((17, id))).unwrap();
        index.note_link(b"b", Some((17, id))).unwrap();
        index.note_link(b"c", Some((17, id ^ (1 << 96)))).unwrap();
        assert_eq!(index.link_identity(b"a").unwrap(), Some((17, id)));
        assert_eq!(
            index.link_groups().unwrap(),
            vec![vec![b"a".to_vec(), b"b".to_vec()]]
        );
        let catalog = crate::catalog::Catalog::open(&temp.path().join("catalog")).unwrap();
        let row = crate::catalog::CachedFile {
            device: 17,
            inode: id,
            size: 1,
            mtime_secs: 0,
            mtime_nanos: 0,
            ctime_secs: 0,
            ctime_nanos: 0,
            mode: 0o100644,
            blob_id: [1; 32],
            xattrs: None,
            sqlite: false,
            cached_at_nanos: 10_000_000_000,
        };
        catalog.cache_file("folder", b"file", &row).unwrap();
        assert_eq!(
            catalog
                .cached_file("folder", b"file")
                .unwrap()
                .unwrap()
                .inode,
            id
        );
    }
}
