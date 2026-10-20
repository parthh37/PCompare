//! File-system primitives shared by the sync and merge executors: safe
//! relative-path resolution, timestamp-preserving atomic copies, staged
//! replacement of existing files, and pluggable disposal of old versions
//! (Recycle Bin / Trash, a versioning folder or the backup store in the app,
//! permanent in tests).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use filetime::FileTime;
use pcompare_core::backups::{BackupBatch, BackupReason};

use crate::baseline::Snap;
use crate::failure::{FailureKind, OpError};

/// Removes files and folders on behalf of an executor.
pub trait Deleter: Send + Sync {
    /// Deletes a file or a whole folder tree.
    fn delete(&self, path: &Path) -> io::Result<()>;

    /// Disposes of `path`, which holds the old version of `rel` on `side`
    /// ("left" / "right") — a deleted item or a replaced file. Returns where
    /// it can be restored from, when it was kept somewhere restorable.
    fn retire(&self, path: &Path, side: &str, rel: &str) -> io::Result<Option<PathBuf>> {
        let _ = (side, rel);
        self.delete(path).map(|()| None)
    }

    /// Disposes of the local file at `path`, which a copy is about to
    /// replace. Returns where it can be restored from, when it was kept
    /// somewhere restorable.
    fn dispose_replaced(&self, path: &Path) -> io::Result<Option<PathBuf>> {
        self.delete(path).map(|()| None)
    }
}

/// Moves items to the OS Recycle Bin / Trash so a sync can be undone by hand.
pub struct TrashDeleter {
    ctx: trash::TrashContext,
}

impl TrashDeleter {
    pub fn new() -> Self {
        #[allow(unused_mut)]
        let mut ctx = trash::TrashContext::new();
        #[cfg(target_os = "macos")]
        {
            // The default asks Finder via AppleScript, which needs an extra
            // automation permission prompt and plays a sound per call.
            use trash::macos::{DeleteMethod, TrashContextExtMacos};
            ctx.set_delete_method(DeleteMethod::NsFileManager);
        }
        Self { ctx }
    }
}

impl Default for TrashDeleter {
    fn default() -> Self {
        Self::new()
    }
}

impl Deleter for TrashDeleter {
    fn delete(&self, path: &Path) -> io::Result<()> {
        self.ctx
            .delete(path)
            .map_err(|e| io::Error::other(format!("cannot move to trash: {e}")))
    }
}

/// Deletes immediately; used by tests, scratch files, and when the user
/// opts out of keeping old versions.
pub struct PermanentDeleter;

impl Deleter for PermanentDeleter {
    fn delete(&self, path: &Path) -> io::Result<()> {
        let meta = fs::symlink_metadata(path)?;
        let remove = || {
            if meta.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            }
        };
        match remove() {
            // Windows refuses to delete read-only files; clear the flag once.
            Err(e) if cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied && meta.permissions().readonly() => {
                let mut perms = meta.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                fs::set_permissions(path, perms)?;
                remove()
            }
            other => other,
        }
    }
}

/// Moves old versions into `<folder>/<stamp>/<side>/<relative path>`, so a
/// sync can be rolled back exactly. One stamp per run keeps runs apart.
pub struct VersioningDeleter {
    root: PathBuf,
}

impl VersioningDeleter {
    pub fn new(folder: impl Into<PathBuf>, stamp: &str) -> Self {
        VersioningDeleter {
            root: folder.into().join(stamp),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Deleter for VersioningDeleter {
    fn delete(&self, path: &Path) -> io::Result<()> {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.retire(path, "other", &name).map(|_| ())
    }

    fn retire(&self, path: &Path, side: &str, rel: &str) -> io::Result<Option<PathBuf>> {
        let target = unused_name(&resolve(&self.root.join(side), rel)?);
        move_path(path, &target)?;
        Ok(Some(target))
    }
}

/// Keeps old versions in PCompare's backup store (see
/// [`pcompare_core::backups`]), recording where each one lived so the
/// Backups page can put it back.
pub struct BackupDeleter {
    batch: Arc<BackupBatch>,
    /// Side label ("left", "right", "output") → that side's root, so
    /// [`Deleter::retire`] knows where an item lived.
    roots: Vec<(String, PathBuf)>,
    /// Where plain deletes go; `None` keeps them as backups too.
    deletes: Option<Box<dyn Deleter>>,
}

impl BackupDeleter {
    pub fn new(batch: Arc<BackupBatch>) -> Self {
        BackupDeleter {
            batch,
            roots: Vec::new(),
            deletes: None,
        }
    }

    /// Names the root of `side`, as passed to [`Deleter::retire`].
    pub fn with_root(mut self, side: &str, root: impl Into<PathBuf>) -> Self {
        self.roots.push((side.to_owned(), root.into()));
        self
    }

    /// Hands plain deletes (Folder Compare's Delete) to `deleter` instead of
    /// keeping them, so only what a copy replaces is backed up.
    pub fn deleting_with(mut self, deleter: Box<dyn Deleter>) -> Self {
        self.deletes = Some(deleter);
        self
    }

    pub fn batch(&self) -> &Arc<BackupBatch> {
        &self.batch
    }
}

impl Deleter for BackupDeleter {
    fn delete(&self, path: &Path) -> io::Result<()> {
        match &self.deletes {
            Some(d) => d.delete(path),
            None => self.batch.keep_move(path, path, BackupReason::Removed).map(|_| ()),
        }
    }

    fn retire(&self, path: &Path, side: &str, rel: &str) -> io::Result<Option<PathBuf>> {
        let original = match self.roots.iter().find(|(s, _)| s == side) {
            Some((_, root)) => resolve(root, rel)?,
            None => path.to_path_buf(),
        };
        // A deleted item is retired where it is; a replaced one from where
        // it was staged aside.
        let reason = if original == path {
            BackupReason::Removed
        } else {
            BackupReason::Replaced
        };
        self.batch.keep_move(path, &original, reason).map(Some)
    }

    fn dispose_replaced(&self, path: &Path) -> io::Result<Option<PathBuf>> {
        self.batch.keep_move(path, path, BackupReason::Replaced).map(Some)
    }
}

/// A file-name-safe UTC timestamp such as `2026-10-12_103000Z`, naming a
/// run's versioning subfolder so runs sort chronologically.
pub fn run_stamp(unix_ms: i64) -> String {
    let secs = unix_ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}_{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `path`, or `name (2).ext`, `name (3).ext`, … if it is taken.
fn unused_name(path: &Path) -> PathBuf {
    if fs::symlink_metadata(path).is_err() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| fs::symlink_metadata(p).is_err())
        .expect("some suffix is free")
}

fn crosses_devices(e: &io::Error) -> bool {
    // ERROR_NOT_SAME_DEVICE on Windows; std maps EXDEV itself.
    e.kind() == io::ErrorKind::CrossesDevices || (cfg!(windows) && e.raw_os_error() == Some(17))
}

/// Moves a file or folder, falling back to copy + delete across volumes.
pub fn move_path(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(from, to) {
        Err(e) if crosses_devices(&e) => {
            if fs::symlink_metadata(from)?.is_dir() {
                copy_tree(from, to)?;
            } else {
                copy_file(from, to, &mut |_| Ok(()))?;
            }
            PermanentDeleter.delete(from)
        }
        other => other,
    }
}

/// Resolves a `/`-separated relative path under `root`, rejecting anything
/// that could escape it (`..`, absolute paths, drive prefixes). Operation
/// lists round-trip through the webview, so they are never trusted blindly.
pub fn resolve(root: &Path, rel: &str) -> io::Result<PathBuf> {
    let mut out = root.to_path_buf();
    let mut any = false;
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        let mut comps = Path::new(part).components();
        match (comps.next(), comps.next()) {
            (Some(Component::Normal(c)), None) => out.push(c),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unsafe relative path `{rel}`"),
                ))
            }
        }
        any = true;
    }
    if !any {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty relative path",
        ));
    }
    Ok(out)
}

pub(crate) static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Marker in the names of temp and staged files, so leftovers from a crash
/// can be recognized (and are hidden on Unix thanks to the leading dot).
pub(crate) const MARKER: &str = ".pcompare-";

fn sibling(dst: &Path, ext: &str) -> PathBuf {
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    dst.with_file_name(format!(".{name}{MARKER}{}-{n}.{ext}", std::process::id()))
}

pub(crate) fn temp_sibling(dst: &Path) -> PathBuf {
    sibling(dst, "tmp")
}

/// Size and modification time of whatever is at `path`, `None` if nothing.
pub fn snap(path: &Path) -> io::Result<Option<Snap>> {
    match fs::symlink_metadata(path) {
        Ok(m) => Ok(Some(snap_of(&m))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) fn snap_of(m: &fs::Metadata) -> Snap {
    let modified_ms = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    Snap {
        size: if m.is_dir() { 0 } else { m.len() },
        modified_ms,
    }
}

/// A finished copy waiting in a temp sibling of its destination.
#[derive(Debug)]
pub struct TempCopy {
    pub tmp: PathBuf,
    pub bytes: u64,
    pub crc: u32,
}

impl TempCopy {
    /// Removes the temp file (after a failure).
    pub fn discard(self) {
        let _ = fs::remove_file(&self.tmp);
    }
}

/// Copies `src` into a temp sibling of `dst`, ready to be renamed into place.
///
/// The data is flushed to disk before returning. The source is examined
/// before and after: if its size or timestamp moved, it was being written
/// while we read it and the copy is rejected as [`FailureKind::SourceChanged`].
/// With `verify`, the temp file is read back and its CRC-32 compared with
/// the one computed while copying ([`FailureKind::VerifyFailed`]), which
/// catches bad sectors, flaky network shares and faulty USB bridges.
pub fn copy_to_temp(
    src: &Path,
    dst: &Path,
    verify: bool,
    on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
) -> Result<TempCopy, OpError> {
    let before = fs::metadata(src).map_err(|e| OpError::io(src, e))?;
    if before.is_dir() {
        return Err(OpError::new(FailureKind::TypeClash, "the source is a folder"));
    }
    let tmp = temp_sibling(dst);
    let result = (|| {
        let (bytes, crc) = write_hashed(src, &tmp, on_bytes)?;
        let after = fs::metadata(src).map_err(|e| OpError::io(src, e))?;
        if snap_of(&after) != snap_of(&before) || bytes != after.len() {
            return Err(OpError {
                path: Some(src.to_path_buf()),
                ..OpError::new(FailureKind::SourceChanged, "the source changed while it was being copied")
            });
        }
        if verify {
            let check = crc_of(&tmp).map_err(|e| OpError::io(&tmp, e))?;
            if check != crc {
                return Err(OpError::new(
                    FailureKind::VerifyFailed,
                    format!("the copy reads back with checksum {check:08x}, expected {crc:08x}"),
                ));
            }
        }
        // Times after the data (the rename keeps them), permissions last so a
        // read-only source doesn't stop us from setting the times.
        filetime::set_file_times(
            &tmp,
            FileTime::from_last_access_time(&before),
            FileTime::from_last_modification_time(&before),
        )
        .map_err(|e| OpError::io(&tmp, e))?;
        fs::set_permissions(&tmp, before.permissions()).map_err(|e| OpError::io(&tmp, e))?;
        Ok(TempCopy { tmp: tmp.clone(), bytes, crc })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_hashed(
    src: &Path,
    tmp: &Path,
    on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
) -> Result<(u64, u32), OpError> {
    let mut input = File::open(src).map_err(|e| OpError::io(src, e))?;
    let mut output = File::create_new(tmp).map_err(|e| OpError::io(tmp, e))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(OpError::io(src, e)),
        };
        output.write_all(&buf[..n]).map_err(|e| OpError::io(tmp, e))?;
        hasher.update(&buf[..n]);
        total += n as u64;
        on_bytes(n as u64).map_err(|e| OpError::io(src, e))?;
    }
    output.sync_all().map_err(|e| OpError::io(tmp, e))?;
    Ok((total, hasher.finalize()))
}

/// CRC-32 of a whole file.
pub fn crc_of(path: &Path) -> io::Result<u32> {
    let mut input = File::open(path)?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match input.read(&mut buf) {
            Ok(0) => return Ok(hasher.finalize()),
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Renames an existing destination to a hidden sibling, making room for its
/// replacement while keeping the old version one rename away.
pub fn stage_aside(dst: &Path) -> io::Result<PathBuf> {
    let staged = sibling(dst, "old");
    fs::rename(dst, &staged)?;
    Ok(staged)
}

/// Removes temp files a crashed copy left next to `dst`.
pub fn remove_stale_temps(dst: &Path) {
    let (Some(dir), Some(name)) = (dst.parent(), dst.file_name()) else {
        return;
    };
    let prefix = format!(".{}{MARKER}", name.to_string_lossy());
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let n = entry.file_name().to_string_lossy().into_owned();
        if n.starts_with(&prefix) && n.ends_with(".tmp") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Copies `src` over `dst`: the data goes to a temporary sibling first and is
/// renamed into place, so readers never see a half-written file and a failed
/// copy leaves the old destination intact. Permissions and access /
/// modification times are carried over. `on_bytes` receives chunk sizes as
/// they are written; returning an error aborts the copy (e.g. cancellation).
/// Returns the number of bytes copied.
pub fn copy_file(
    src: &Path,
    dst: &Path,
    on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
) -> io::Result<u64> {
    let meta = fs::metadata(src)?;
    if meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source is a folder",
        ));
    }
    if fs::metadata(dst).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a folder with this name exists at the destination",
        ));
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp = temp_sibling(dst);
    let result =
        write_temp(src, &tmp, &meta, on_bytes).and_then(|n| fs::rename(&tmp, dst).map(|_| n));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_temp(
    src: &Path,
    tmp: &Path,
    meta: &fs::Metadata,
    on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
) -> io::Result<u64> {
    let mut input = File::open(src)?;
    let mut output = File::create_new(tmp)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        output.write_all(&buf[..n])?;
        total += n as u64;
        on_bytes(n as u64)?;
    }
    output.sync_all()?;
    drop(output);
    // Times after the data (the rename keeps them), permissions last so a
    // read-only source doesn't stop us from setting the times.
    filetime::set_file_times(
        tmp,
        FileTime::from_last_access_time(meta),
        FileTime::from_last_modification_time(meta),
    )?;
    fs::set_permissions(tmp, meta.permissions())?;
    Ok(total)
}

/// Recursively copies a folder (used when a whole subtree is taken from one
/// side). Returns the number of bytes copied.
pub fn copy_tree(src: &Path, dst: &Path) -> io::Result<u64> {
    fs::create_dir_all(dst)?;
    let mut total = 0;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if fs::metadata(entry.path())?.is_dir() {
            total += copy_tree(&entry.path(), &target)?;
        } else {
            total += copy_file(&entry.path(), &target, &mut |_| Ok(()))?;
        }
    }
    Ok(total)
}

/// Canonicalizes a path that may not exist yet: the longest existing prefix
/// is resolved and the remaining components are appended verbatim.
pub fn canonical_lenient(path: &Path) -> io::Result<PathBuf> {
    let mut missing = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        match cur.canonicalize() {
            Ok(mut base) => {
                base.extend(missing.iter().rev());
                return Ok(base);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let name = cur.file_name().map(|n| n.to_owned()).ok_or(e)?;
                missing.push(name);
                if !cur.pop() || cur.as_os_str().is_empty() {
                    cur = std::env::current_dir()?;
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// True when both files exist with identical contents. Sizes are checked
/// first; equal timestamps are deliberately not trusted, since a wrong
/// "identical" verdict would silently drop a merged change.
pub fn same_contents(a: &Path, b: &Path) -> io::Result<bool> {
    let (ma, mb) = match (fs::metadata(a), fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => (ma, mb),
        _ => return Ok(false),
    };
    if ma.is_dir() || mb.is_dir() || ma.len() != mb.len() {
        return Ok(false);
    }
    pcompare_core::folder::files_equal(a, b).map_err(|e| io::Error::other(e.to_string()))
}

// ---------- attributes, NTFS security and short names ----------

/// Attribute bits to switch on and off. DOS bits ([`pcompare_core::folder::attr`])
/// apply on Windows; on Unix only read-only maps (onto the write bits), and
/// the `mode_*` masks change permission bits directly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AttrChange {
    pub set: u32,
    pub clear: u32,
    pub mode_set: u32,
    pub mode_clear: u32,
}

impl AttrChange {
    pub fn is_empty(&self) -> bool {
        self.set == 0 && self.clear == 0 && self.mode_set == 0 && self.mode_clear == 0
    }
}

/// Applies `change` to a local file or folder (not following links).
pub fn set_attributes(path: &Path, change: &AttrChange) -> io::Result<()> {
    #[cfg(windows)]
    {
        win::set_attributes(path, change)
    }
    #[cfg(not(windows))]
    {
        use pcompare_core::folder::attr;
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Ok(()); // Unix links carry no permissions of their own
        }
        let mut perms = meta.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut mode = perms.mode() & 0o7777;
            mode = (mode | change.mode_set) & !change.mode_clear;
            if change.set & attr::READONLY != 0 {
                mode &= !0o222;
            } else if change.clear & attr::READONLY != 0 && mode & 0o222 == 0 {
                mode |= 0o200;
            }
            perms.set_mode(mode);
        }
        #[cfg(not(unix))]
        if change.set & attr::READONLY != 0 {
            perms.set_readonly(true);
        }
        fs::set_permissions(path, perms)
    }
}

/// Copies the owner, group and access control list of `src` onto `dst`
/// (Windows NTFS). Best effort: setting another account as owner needs the
/// restore privilege (an elevated process), so without it only the ACL is
/// copied and the returned note says so. Elsewhere this does nothing.
pub fn copy_security(src: &Path, dst: &Path) -> io::Result<Option<String>> {
    #[cfg(windows)]
    {
        win::copy_security(src, dst)
    }
    #[cfg(not(windows))]
    {
        let _ = (src, dst);
        Ok(None)
    }
}

/// Gives `dst` the DOS 8.3 short name `src` has, when it differs from the
/// one Windows generated (old installers and scripts may refer to files by
/// it). Needs the restore privilege; best effort. Elsewhere this does nothing.
pub fn copy_short_name(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        win::copy_short_name(src, dst)
    }
    #[cfg(not(windows))]
    {
        let _ = (src, dst);
        Ok(())
    }
}

/// Gives `dst` the creation date ("birth time") of `src`
/// ("Copy creation dates"). Windows and macOS let it be set; elsewhere
/// this does nothing.
pub fn copy_created(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        #[cfg(target_os = "macos")]
        use std::os::macos::fs::FileTimesExt;
        #[cfg(windows)]
        use std::os::windows::fs::FileTimesExt;
        let created = fs::metadata(src)?.created()?;
        open_for_times(dst)?.set_times(fs::FileTimes::new().set_created(created))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (src, dst);
        Ok(())
    }
}

/// A handle that may change `path`'s times, for files and folders alike
/// (read-only files included: times are attributes, not data).
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn open_for_times(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES};
        fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }
    #[cfg(target_os = "macos")]
    {
        File::open(path)
    }
}

/// Gives the folder `dst` the access and modification times of `src` (and,
/// with `created`, its creation date).
pub fn copy_folder_times(src: &Path, dst: &Path, times: bool, created: bool) -> io::Result<()> {
    if times {
        let m = fs::metadata(src)?;
        filetime::set_file_times(dst, FileTime::from_last_access_time(&m), FileTime::from_last_modification_time(&m))?;
    }
    if created {
        copy_created(src, dst)?;
    }
    Ok(())
}

/// Copies whichever of ACLs and short names are asked for, after `dst` was
/// written from `src`. Returns notes about anything that couldn't be copied;
/// the data copy itself has succeeded either way.
pub fn copy_extras(src: &Path, dst: &Path, acls: bool, short_names: bool) -> Vec<String> {
    let mut notes = Vec::new();
    if acls {
        match copy_security(src, dst) {
            Ok(Some(note)) => notes.push(note),
            Ok(None) => {}
            Err(e) => notes.push(format!("permissions (ACL) not copied: {e}")),
        }
    }
    if short_names {
        if let Err(e) = copy_short_name(src, dst) {
            notes.push(format!("short (8.3) name not copied: {e}"));
        }
    }
    notes
}

#[cfg(windows)]
mod win {
    use std::ffi::OsString;
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};
    use std::sync::Once;

    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LUID};
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, GetFileSecurityW, LookupPrivilegeValueW, SetFileSecurityW,
        DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, LUID_AND_ATTRIBUTES,
        OWNER_SECURITY_INFORMATION, SE_BACKUP_NAME, SE_PRIVILEGE_ENABLED, SE_RESTORE_NAME,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileAttributesW, GetShortPathNameW, SetFileAttributesW, SetFileShortNameW, DELETE,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, INVALID_FILE_ATTRIBUTES, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::AttrChange;
    use pcompare_core::folder::attr;

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    /// Enables the backup and restore privileges once, if the account holds
    /// them (administrators do, in an elevated process). Without them the
    /// calls below still work for what the user may change anyway.
    fn enable_privileges() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| unsafe {
            let mut token: HANDLE = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) == 0 {
                return;
            }
            for name in [SE_RESTORE_NAME, SE_BACKUP_NAME] {
                let mut luid = LUID { LowPart: 0, HighPart: 0 };
                if LookupPrivilegeValueW(null(), name, &mut luid) == 0 {
                    continue;
                }
                let tp = TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
                };
                // Fails quietly (ERROR_NOT_ALL_ASSIGNED) when the privilege isn't held.
                AdjustTokenPrivileges(token, 0, &tp, 0, null_mut(), null_mut());
            }
            CloseHandle(token);
        });
    }

    pub fn set_attributes(path: &Path, change: &AttrChange) -> io::Result<()> {
        let w = wide(path);
        let old = unsafe { GetFileAttributesW(w.as_ptr()) };
        if old == INVALID_FILE_ATTRIBUTES {
            return Err(io::Error::last_os_error());
        }
        let dos = attr::DOS_MASK;
        let new = (old | (change.set & dos)) & !(change.clear & dos);
        if new == old {
            return Ok(());
        }
        if unsafe { SetFileAttributesW(w.as_ptr(), new) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// A self-relative security descriptor, 8-byte aligned.
    fn read_security(path: &[u16], info: u32) -> io::Result<Vec<u64>> {
        let mut need = 0u32;
        unsafe { GetFileSecurityW(path.as_ptr(), info, null_mut(), 0, &mut need) };
        if need == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u64; (need as usize).div_ceil(8)];
        let len = (buf.len() * 8) as u32;
        if unsafe { GetFileSecurityW(path.as_ptr(), info, buf.as_mut_ptr().cast(), len, &mut need) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(buf)
    }

    pub fn copy_security(src: &Path, dst: &Path) -> io::Result<Option<String>> {
        enable_privileges();
        let (s, d) = (wide(src), wide(dst));
        let full = OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let mut sd = read_security(&s, full)?;
        if unsafe { SetFileSecurityW(d.as_ptr(), full, sd.as_mut_ptr().cast()) } != 0 {
            return Ok(None);
        }
        let owner_err = io::Error::last_os_error();
        // Taking over another account's ownership needs the restore privilege;
        // the access rules themselves can still be copied.
        if unsafe { SetFileSecurityW(d.as_ptr(), DACL_SECURITY_INFORMATION, sd.as_mut_ptr().cast()) } != 0 {
            return Ok(Some(format!(
                "permissions copied, but not the owner ({owner_err}); run PCompare as administrator to copy owners"
            )));
        }
        Err(io::Error::last_os_error())
    }

    /// The 8.3 alias of `p`'s last component, `None` when it has none of its
    /// own (8.3 names disabled, or the long name already fits 8.3).
    fn short_name(p: &Path) -> io::Result<Option<String>> {
        let w = wide(p);
        let n = unsafe { GetShortPathNameW(w.as_ptr(), null_mut(), 0) };
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u16; n as usize];
        let n = unsafe { GetShortPathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        let short = PathBuf::from(OsString::from_wide(&buf[..n as usize]));
        let name = short.file_name().map(|n| n.to_string_lossy().into_owned());
        let long = p.file_name().map(|n| n.to_string_lossy().into_owned());
        Ok(name.filter(|s| Some(s.to_uppercase()) != long.as_ref().map(|l| l.to_uppercase())))
    }

    pub fn copy_short_name(src: &Path, dst: &Path) -> io::Result<()> {
        let Some(want) = short_name(src)? else {
            return Ok(());
        };
        if short_name(dst)?.is_some_and(|have| have.eq_ignore_ascii_case(&want)) {
            return Ok(());
        }
        enable_privileges();
        let d = wide(dst);
        let handle = unsafe {
            CreateFileW(
                d.as_ptr(),
                GENERIC_WRITE | DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let name: Vec<u16> = std::ffi::OsStr::new(&want).encode_wide().chain(Some(0)).collect();
        let ok = unsafe { SetFileShortNameW(handle, name.as_ptr()) };
        let err = io::Error::last_os_error();
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return Err(err);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn run_stamp_formats_utc() {
        assert_eq!(run_stamp(0), "1970-01-01_000000Z");
        assert_eq!(run_stamp(1_760_268_645_000), "2025-10-12_113045Z");
        assert_eq!(run_stamp(951_782_400_000), "2000-02-29_000000Z");
    }

    #[test]
    fn resolve_rejects_escapes() {
        let root = Path::new("/r");
        assert_eq!(resolve(root, "a/b.txt").unwrap(), Path::new("/r/a/b.txt"));
        assert!(resolve(root, "../x").is_err());
        assert!(resolve(root, "a/../../x").is_err());
        assert!(resolve(root, "").is_err());
        assert!(resolve(root, "./a").is_err());
    }

    #[test]
    fn copy_preserves_mtime_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        fs::write(&src, b"new contents").unwrap();
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        filetime::set_file_mtime(&src, FileTime::from_system_time(when)).unwrap();

        let dst = dir.path().join("deep/nested/dst.txt");
        let mut seen = 0;
        let n = copy_file(&src, &dst, &mut |b| {
            seen += b;
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 12);
        assert_eq!(seen, 12);
        assert_eq!(fs::read(&dst).unwrap(), b"new contents");
        assert_eq!(fs::metadata(&dst).unwrap().modified().unwrap(), when);

        // Overwrite, and no temp files are left behind.
        fs::write(&src, b"v2").unwrap();
        copy_file(&src, &dst, &mut |_| Ok(())).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"v2");
        let names: Vec<_> = fs::read_dir(dst.parent().unwrap()).unwrap().collect();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn canonical_lenient_handles_missing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/b");
        let c = canonical_lenient(&p).unwrap();
        assert!(c.ends_with("a/b"));
        assert_eq!(
            c.parent().unwrap().parent().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }
}
