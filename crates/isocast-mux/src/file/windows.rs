use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_TYPE_DISK, GetFileInformationByHandle, GetFileType,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use super::OpenMode;

pub(super) fn open_directory(path: &Path) -> io::Result<File> {
    // Only this initial resolution may follow a symlink/junction. Permit a
    // directory rename: RootDirectory below still refers to the same object.
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording folder is not a directory",
        ));
    }
    Ok(file)
}

pub(super) fn open_child(directory: &File, name: &OsStr, mode: OpenMode) -> io::Result<File> {
    let mut wide: Vec<u16> = name.encode_wide().collect();
    let length = wide
        .len()
        .checked_mul(2)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "child name is too long"))?;
    let name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: directory.as_raw_handle(),
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    };
    let (access, sharing, disposition) = match mode {
        // FILE_CREATE is NT's atomic create_new, not OPEN_IF or overwrite.
        OpenMode::Create => (
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ,
            FILE_CREATE,
        ),
        OpenMode::Repair => (
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ,
            FILE_OPEN,
        ),
        OpenMode::Read => (
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
        ),
        // Several recordings need to open the same lock for LockFileEx's
        // shared lock. Deny delete sharing, so nobody can replace its name
        // and thereby create an independent folder-lock domain.
        OpenMode::Lock => (
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN_IF,
        ),
    };
    let mut handle = null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: all pointers outlive the synchronous call; the counted name
    // is one validated child (no slash, colon, root or device syntax), and
    // the RootDirectory handle remains open. No OBJ_INHERIT is specified.
    //
    // NtCreateFile is a synchronous create API: IRP_MJ_CREATE carries
    // IRP_SYNCHRONOUS_API, even if a lower driver pends its IRP. The I/O
    // manager completes the create before returning to user mode. In
    // addition FILE_SYNCHRONOUS_IO_NONALERT (with SYNCHRONIZE included in
    // FILE_GENERIC_*) makes every subsequent I/O synchronous/non-alertable.
    // There is therefore no outstanding operation retaining this stack
    // IOSB on return. Do not change these to overlapped/alertable options.
    // See Microsoft's NtCreateFile and IRP_MJ_CREATE API documentation.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut status_block,
            null(),
            FILE_ATTRIBUTE_NORMAL,
            sharing,
            disposition,
            FILE_OPEN_REPARSE_POINT | FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
            null(),
            0,
        )
    };
    if status < 0 {
        // SAFETY: a pure conversion of the completed NT error status.
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    // SAFETY: a successful synchronous NtCreateFile returns a new owned
    // handle. File closes it on validation failure as well as on normal Drop.
    let file = unsafe { File::from_raw_handle(handle) };
    validate_file(&file)?;
    // For writable media, FILE_SHARE_READ is the lifetime ownership guard:
    // any competing write/delete open conflicts, including existing handles
    // and opens through hard-link/short-name aliases. Repair requests WRITE
    // with that same sharing policy, so cannot acquire a live writer's file.
    // Unlike LockFileEx on media bytes, it permits normal read-only probes
    // (the reader must share WRITE, as for any file being written on Windows).
    // The guard is released by closing this handle or process termination.
    Ok(file)
}

pub(super) fn validate_file(file: &File) -> io::Result<()> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file owns a live handle and info is a writable output structure.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Metadata is queried on the opened object, never through its path.
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reparse points (including cloud/sync placeholders) are not supported; choose a local, non-synced recording folder",
        ));
    }
    // FILE_NON_DIRECTORY_FILE alone also admits devices, hence FILE_TYPE_DISK.
    // SAFETY: file owns a live handle throughout the query.
    let kind = unsafe { GetFileType(file.as_raw_handle()) };
    if kind != FILE_TYPE_DISK
        || info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_DEVICE) != 0
        || info.nNumberOfLinks != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "child must be a regular disk file with exactly one link and no reparse point",
        ));
    }
    Ok(())
}

pub(super) fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    file.seek_read(buf, offset)
}

pub(super) fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    file.seek_write(buf, offset)
}
