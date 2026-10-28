//! Small, audited wrappers around the Win32 file APIs used by the cleaner.
//!
//! `std::fs` is used for enumeration and metadata. Handles created here are
//! always opened with `FILE_FLAG_OPEN_REPARSE_POINT`, so opening a link never
//! resolves to its target. Every unsafe call is contained in this module.

use std::ffi::c_void;
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo, GetFileInformationByHandle,
    GetFinalPathNameByHandleW, SetFileInformationByHandle, VOLUME_NAME_DOS,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId {
    pub volume: u32,
    pub index: u64,
}

pub fn is_reparse_point(metadata: &Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub fn is_read_only(metadata: &Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_READONLY != 0
}

/// Directory detection that also works for a junction's metadata, where
/// `is_dir()` can be false because the entry itself is a link.
pub fn is_directory(metadata: &Metadata) -> bool {
    metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0
}

fn open_with(path: &Path, access: u32, share: u32, flags: u32) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(access)
        .share_mode(share)
        .custom_flags(flags)
        .open(path)
}

/// Opens a directory handle without following a reparse point.
///
/// `FILE_SHARE_DELETE` is intentionally omitted: while this handle is alive,
/// another handle cannot rename or delete the directory. This is used for the
/// selected root for the duration of a cleanup.
pub fn open_root_guard(path: &Path) -> io::Result<File> {
    open_with(
        path,
        FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )
}

/// Opens a directory just long enough to validate its identity and final path.
pub fn open_directory_check(path: &Path) -> io::Result<File> {
    open_with(
        path,
        FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )
}

/// Non-destructive probe: does this process have DELETE access to the root?
pub fn open_root_delete_probe(path: &Path) -> io::Result<File> {
    open_with(
        path,
        DELETE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )
}

/// Opens an entry with DELETE access without following reparse points.
pub fn open_for_delete(path: &Path) -> io::Result<File> {
    open_with(
        path,
        DELETE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )
}

pub fn file_id(file: &File) -> io::Result<FileId> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let handle = file.as_raw_handle() as HANDLE;

    // SAFETY: `handle` is a live handle owned by `file`; `info` is a valid,
    // correctly sized output buffer for GetFileInformationByHandle.
    let result = unsafe { GetFileInformationByHandle(handle, &mut info) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(FileId {
        volume: info.dwVolumeSerialNumber,
        index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}

/// Returns the normalized final path behind an open handle.
pub fn final_path(file: &File) -> io::Result<PathBuf> {
    let handle = file.as_raw_handle() as HANDLE;
    let mut buffer = vec![0u16; 512];

    loop {
        // SAFETY: `handle` is live for the duration of the call. The output
        // buffer is valid for its stated length.
        let written = unsafe {
            GetFinalPathNameByHandleW(
                handle,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };

        if written == 0 {
            return Err(io::Error::last_os_error());
        }

        if written as usize >= buffer.len() {
            let needed = written as usize + 1;
            if needed > 32_768 {
                return Err(io::Error::other("最终路径过长"));
            }
            buffer.resize(needed, 0);
            continue;
        }

        buffer.truncate(written as usize);
        return Ok(PathBuf::from(strip_verbatim(&buffer)));
    }
}

/// Marks an open file or empty directory for deletion. Closing the handle
/// completes the deletion.
pub fn mark_for_delete(file: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    let handle = file.as_raw_handle() as HANDLE;

    // SAFETY: `handle` is live and has DELETE access. `info` is the documented
    // FILE_DISPOSITION_INFO structure and the size matches the class.
    let result = unsafe {
        SetFileInformationByHandle(
            handle,
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast::<c_void>(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };

    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn strip_verbatim(wide: &[u16]) -> std::ffi::OsString {
    const VERBATIM: &[u16] = &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const VERBATIM_UNC: &[u16] = &[
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ];
    const NT_PREFIX: &[u16] = &[b'\\' as u16, b'?' as u16, b'?' as u16, b'\\' as u16];

    if wide.starts_with(VERBATIM_UNC) {
        let mut result = vec![b'\\' as u16, b'\\' as u16];
        result.extend_from_slice(&wide[VERBATIM_UNC.len()..]);
        std::ffi::OsString::from_wide(&result)
    } else if wide.starts_with(VERBATIM) {
        std::ffi::OsString::from_wide(&wide[VERBATIM.len()..])
    } else if wide.starts_with(NT_PREFIX) {
        std::ffi::OsString::from_wide(&wide[NT_PREFIX.len()..])
    } else {
        std::ffi::OsString::from_wide(wide)
    }
}

/// Component-wise check that `candidate` is strictly inside `root`.
///
/// Both paths should come from `GetFinalPathNameByHandleW`, which gives the
/// stable on-disk casing. A plain string prefix comparison is never used.
pub fn is_strictly_inside(root: &Path, candidate: &Path) -> bool {
    match candidate.strip_prefix(root) {
        Ok(rest) => !rest.as_os_str().is_empty(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_verbatim_prefixes() {
        let input: Vec<u16> = "\\\\?\\C:\\Windows\\Temp".encode_utf16().collect();
        assert_eq!(
            PathBuf::from(strip_verbatim(&input)),
            PathBuf::from(r"C:\Windows\Temp")
        );

        let unc: Vec<u16> = "\\\\?\\UNC\\server\\share".encode_utf16().collect();
        assert_eq!(
            PathBuf::from(strip_verbatim(&unc)),
            PathBuf::from(r"\\server\share")
        );
    }

    #[test]
    fn boundary_is_component_wise() {
        let root = Path::new(r"C:\Users\a\Temp");
        assert!(is_strictly_inside(
            root,
            Path::new(r"C:\Users\a\Temp\child")
        ));
        assert!(!is_strictly_inside(root, root));
        assert!(!is_strictly_inside(
            root,
            Path::new(r"C:\Users\a\TempEvil\child")
        ));
    }
}
