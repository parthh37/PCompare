//! Why an operation failed, in terms the sync can act on: retry it, defer
//! it, skip it, or stop the run. Raw OS errors are classified per platform,
//! and for locked files Windows can tell us *who* holds the lock.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailureKind {
    /// Opened exclusively or byte-range locked by another process.
    Locked,
    /// Access denied. On Windows often transient (antivirus scan, a pending
    /// delete, the search indexer); elsewhere usually a real permission issue.
    AccessDenied,
    /// The source (or a parent folder) vanished after the preview.
    NotFound,
    DiskFull,
    ReadOnly,
    /// Name or path too long, or invalid on the target file system.
    BadName,
    /// A network share or cloud-sync provider dropped out.
    Network,
    /// The target was modified after the preview; overwriting or deleting it
    /// now could destroy work the plan never saw. Preview again.
    ChangedSincePreview,
    /// The source kept changing while it was copied.
    SourceChanged,
    /// The copy read back with a different checksum than the source.
    VerifyFailed,
    /// Something of the other type (file vs folder) is in the way.
    TypeClash,
    /// The operation itself is invalid (e.g. an unsafe relative path).
    Invalid,
    Cancelled,
    Other,
}

impl FailureKind {
    /// Short label for reports and the UI.
    pub fn label(self) -> &'static str {
        match self {
            FailureKind::Locked => "File in use",
            FailureKind::AccessDenied => "Access denied",
            FailureKind::NotFound => "Not found",
            FailureKind::DiskFull => "Disk full",
            FailureKind::ReadOnly => "Read-only",
            FailureKind::BadName => "Invalid name or path too long",
            FailureKind::Network => "Network error",
            FailureKind::ChangedSincePreview => "Changed since preview",
            FailureKind::SourceChanged => "Source changed during copy",
            FailureKind::VerifyFailed => "Verification failed",
            FailureKind::TypeClash => "File/folder clash",
            FailureKind::Invalid => "Invalid operation",
            FailureKind::Cancelled => "Cancelled",
            FailureKind::Other => "Error",
        }
    }

    /// Failures that hit every remaining operation alike, so continuing the
    /// run only produces a wall of identical errors.
    pub fn is_fatal_for_run(self) -> bool {
        matches!(self, FailureKind::DiskFull | FailureKind::ReadOnly)
    }
}

/// A failure with its classification, carried through the executor.
#[derive(Debug)]
pub struct OpError {
    pub kind: FailureKind,
    pub message: String,
    /// The file the failure is about (the source or the target), so locked
    /// files can be traced to the process holding them.
    pub path: Option<PathBuf>,
}

impl OpError {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        OpError {
            kind,
            message: message.into(),
            path: None,
        }
    }

    /// Classifies an I/O error that happened on `path`.
    pub fn io(path: &Path, e: io::Error) -> Self {
        OpError {
            path: Some(path.to_path_buf()),
            ..e.into()
        }
    }
}

impl From<io::Error> for OpError {
    fn from(e: io::Error) -> Self {
        OpError {
            kind: classify(&e),
            message: e.to_string(),
            path: None,
        }
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Maps an I/O error to a [`FailureKind`].
pub fn classify(e: &io::Error) -> FailureKind {
    if let Some(kind) = e.raw_os_error().and_then(classify_os) {
        return kind;
    }
    use io::ErrorKind as K;
    match e.kind() {
        K::Interrupted => FailureKind::Cancelled,
        K::NotFound => FailureKind::NotFound,
        K::PermissionDenied => FailureKind::AccessDenied,
        K::ResourceBusy | K::ExecutableFileBusy | K::WouldBlock => FailureKind::Locked,
        K::StorageFull | K::QuotaExceeded | K::FileTooLarge => FailureKind::DiskFull,
        K::ReadOnlyFilesystem => FailureKind::ReadOnly,
        K::InvalidFilename => FailureKind::BadName,
        K::IsADirectory | K::NotADirectory | K::AlreadyExists => FailureKind::TypeClash,
        K::InvalidInput => FailureKind::Invalid,
        K::TimedOut
        | K::StaleNetworkFileHandle
        | K::NetworkDown
        | K::NetworkUnreachable
        | K::HostUnreachable
        | K::ConnectionReset
        | K::ConnectionAborted
        | K::BrokenPipe => FailureKind::Network,
        _ => FailureKind::Other,
    }
}

/// Win32 codes that `std` maps to an unhelpful or generic `ErrorKind`.
#[cfg(windows)]
fn classify_os(code: i32) -> Option<FailureKind> {
    Some(match code {
        // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION, ERROR_USER_MAPPED_FILE
        32 | 33 | 1224 => FailureKind::Locked,
        // ERROR_ACCESS_DENIED, ERROR_DELETE_PENDING
        5 | 303 => FailureKind::AccessDenied,
        // ERROR_HANDLE_DISK_FULL, ERROR_DISK_FULL, ERROR_DISK_QUOTA_EXCEEDED
        39 | 112 | 1295 => FailureKind::DiskFull,
        // ERROR_WRITE_PROTECT
        19 => FailureKind::ReadOnly,
        // ERROR_INVALID_NAME, ERROR_FILENAME_EXCED_RANGE
        123 | 206 => FailureKind::BadName,
        // ERROR_BAD_NETPATH, ERROR_UNEXP_NET_ERR, ERROR_NETNAME_DELETED,
        // ERROR_SEM_TIMEOUT, ERROR_CLOUD_FILE_PROVIDER_NOT_RUNNING
        53 | 59 | 64 | 121 | 362 => FailureKind::Network,
        _ => return None,
    })
}

#[cfg(not(windows))]
fn classify_os(_code: i32) -> Option<FailureKind> {
    None // std's errno mapping is precise enough
}

/// A process holding a file open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockHolder {
    pub pid: u32,
    pub name: String,
}

/// Lists the processes that have `path` open, where the OS can tell.
/// Windows asks the Restart Manager (the API installers use to say "close
/// Excel to continue"); other platforms return nothing.
pub fn lock_holders(path: &Path) -> Vec<LockHolder> {
    #[cfg(windows)]
    {
        restart_manager::holders(path).unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Vec::new()
    }
}

#[cfg(windows)]
mod restart_manager {
    use super::LockHolder;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows_sys::Win32::System::RestartManager::{
        RmEndSession, RmGetList, RmRegisterResources, RmStartSession, CCH_RM_SESSION_KEY,
        RM_PROCESS_INFO,
    };

    /// Ends the Restart Manager session however we leave `holders`.
    struct Session(u32);
    impl Drop for Session {
        fn drop(&mut self) {
            unsafe { RmEndSession(self.0) };
        }
    }

    fn wide_to_string(w: &[u16]) -> String {
        let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
        String::from_utf16_lossy(&w[..end])
    }

    pub fn holders(path: &Path) -> Option<Vec<LockHolder>> {
        let mut handle = 0u32;
        let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        if unsafe { RmStartSession(&mut handle, 0, key.as_mut_ptr()) } != ERROR_SUCCESS {
            return None;
        }
        let session = Session(handle);

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let files = [wide.as_ptr()];
        let rc = unsafe {
            RmRegisterResources(session.0, 1, files.as_ptr(), 0, std::ptr::null(), 0, std::ptr::null())
        };
        if rc != ERROR_SUCCESS {
            return None;
        }

        // The holder list can grow between calls; retry a few times.
        let mut infos: Vec<RM_PROCESS_INFO> = Vec::new();
        for _ in 0..4 {
            let mut needed = 0u32;
            let mut count = infos.len() as u32;
            let mut reasons = 0u32;
            let rc = unsafe {
                RmGetList(session.0, &mut needed, &mut count, infos.as_mut_ptr(), &mut reasons)
            };
            if rc == ERROR_SUCCESS {
                infos.truncate(count as usize);
                return Some(
                    infos
                        .iter()
                        .map(|p| LockHolder {
                            pid: p.Process.dwProcessId,
                            name: wide_to_string(&p.strAppName),
                        })
                        .collect(),
                );
            }
            if rc != ERROR_MORE_DATA {
                return None;
            }
            // SAFETY: RM_PROCESS_INFO is plain old data; zeroed is a valid value.
            infos = vec![unsafe { std::mem::zeroed() }; needed as usize];
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_errors() {
        let k = |kind| classify(&io::Error::from(kind));
        assert_eq!(k(io::ErrorKind::NotFound), FailureKind::NotFound);
        assert_eq!(k(io::ErrorKind::PermissionDenied), FailureKind::AccessDenied);
        assert_eq!(k(io::ErrorKind::StorageFull), FailureKind::DiskFull);
        assert_eq!(k(io::ErrorKind::ResourceBusy), FailureKind::Locked);
        assert_eq!(k(io::ErrorKind::Interrupted), FailureKind::Cancelled);
    }

    #[cfg(unix)]
    #[test]
    fn classifies_errno() {
        let os = |n| classify(&io::Error::from_raw_os_error(n));
        assert_eq!(os(16), FailureKind::Locked); // EBUSY
        assert_eq!(os(28), FailureKind::DiskFull); // ENOSPC
        assert_eq!(os(30), FailureKind::ReadOnly); // EROFS
        assert_eq!(os(13), FailureKind::AccessDenied); // EACCES
    }

    #[cfg(windows)]
    #[test]
    fn classifies_win32_codes() {
        let os = |n| classify(&io::Error::from_raw_os_error(n));
        assert_eq!(os(32), FailureKind::Locked);
        assert_eq!(os(33), FailureKind::Locked);
        assert_eq!(os(112), FailureKind::DiskFull);
        assert_eq!(os(206), FailureKind::BadName);
    }
}
