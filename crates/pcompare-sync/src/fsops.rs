//! File-system primitives shared by the sync and merge executors: safe
//! relative-path resolution, timestamp-preserving atomic copies, and
//! pluggable deletion (Recycle Bin / Trash in the app, permanent in tests).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use filetime::FileTime;

/// Removes files and folders on behalf of an executor.
pub trait Deleter: Send + Sync {
    /// Deletes a file or a whole folder tree.
    fn delete(&self, path: &Path) -> io::Result<()>;
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

/// Deletes immediately; used by tests and for scratch files.
pub struct PermanentDeleter;

impl Deleter for PermanentDeleter {
    fn delete(&self, path: &Path) -> io::Result<()> {
        if fs::symlink_metadata(path)?.is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        }
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

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_sibling(dst: &Path) -> PathBuf {
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    dst.with_file_name(format!(".{name}.pcompare-{}-{n}.tmp", std::process::id()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

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
