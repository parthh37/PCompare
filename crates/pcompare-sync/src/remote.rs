//! Folder Sync with a remote side (SFTP, FTP, S3, cloud drives, ...).
//!
//! A sync side is a [`Root`]: a local folder, or a folder on a writable
//! [`VfsWrite`]. Local sides keep every guarantee of [`crate::exec`]; remote
//! sides get as many as the back end allows:
//!
//! - **No torn files.** Where the server can rename, an upload goes to a
//!   temp name beside its target and is swapped into place, with the file it
//!   replaces staged aside first (and journaled), so a crash leaves one
//!   version or the other recoverable. Back ends without rename (object
//!   stores; FTP, which OpenDAL already writes via a temp name) commit a
//!   write all at once. Either way the result is checked by size, and by
//!   checksum when verification is on.
//! - **Downloads** land in a temp sibling and then go through the same
//!   staged replacement, Recycle Bin and versioning as a local copy.
//! - **Resumable transfers.** A transfer cut off by the network keeps its
//!   partial data and the automatic retry continues where it stopped: ranged
//!   reads for downloads, offset writes for uploads where the server allows
//!   them (SFTP). Other uploads start over.
//! - **Remote deletions are permanent.** Remote folders have no Recycle Bin,
//!   so files deleted or replaced there can't be rolled back; the plan says
//!   so up front ([`remote_warnings`]). Rollback still removes files and
//!   folders a sync created on a remote.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use filetime::FileTime;
use pcompare_core::vfs_write::VfsWrite;

use crate::baseline::Snap;
use crate::exec::{self, Done, PreflightIssue, Recovery, RollbackItem, RollbackResult, Runner};
use crate::failure::{FailureKind, OpError};
use crate::fsops::{self, Deleter, TempCopy};
use crate::sync::{Direction, OpKind, PlanWarning, SyncOp, SyncPlan, WarningKind};

/// One side of a sync.
#[derive(Clone, Copy)]
pub enum Root<'a> {
    Local(&'a Path),
    /// A folder on a writable file system: its path there, and how it is
    /// shown to the user (e.g. `remote://my-sftp/www`).
    Remote {
        fs: &'a dyn VfsWrite,
        path: &'a str,
        label: &'a str,
    },
}

impl Root<'_> {
    pub fn is_remote(&self) -> bool {
        matches!(self, Root::Remote { .. })
    }

    fn label(&self) -> String {
        match self {
            Root::Local(p) => p.display().to_string(),
            Root::Remote { label, .. } => (*label).to_owned(),
        }
    }
}

/// Transfers of one operation cut short by the network, kept so the next
/// attempt can resume them. Remote → remote copies have both halves.
#[derive(Default)]
pub(crate) struct Partial<'a> {
    /// Local temp file holding the data downloaded so far, and the source it
    /// came from.
    download: Option<(PathBuf, Snap)>,
    /// Remote temp file holding the data uploaded so far, and the local file
    /// it came from.
    upload: Option<(&'a dyn VfsWrite, String, Snap)>,
}

/// `root` + `rel` in the file system's own syntax. Operation lists
/// round-trip through the webview, so `.` / `..` components are refused.
fn remote_path(fs: &dyn VfsWrite, root: &str, rel: &str) -> Result<String, OpError> {
    let mut out = root.to_owned();
    let mut any = false;
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        if part == "." || part == ".." {
            return Err(OpError::new(
                FailureKind::Invalid,
                format!("unsafe relative path `{rel}`"),
            ));
        }
        out = fs.join(&out, part);
        any = true;
    }
    if !any {
        return Err(OpError::new(FailureKind::Invalid, "empty relative path"));
    }
    Ok(out)
}

/// `("a/b", "c.txt")` for `"a/b/c.txt"`.
fn split_rel(rel: &str) -> (&str, &str) {
    rel.trim_matches('/')
        .rsplit_once('/')
        .unwrap_or(("", rel.trim_matches('/')))
}

/// A hidden temp name next to `rel`, in the same pattern as local temp
/// files so crash recovery recognizes it.
fn temp_path(fs: &dyn VfsWrite, root: &str, rel: &str, ext: &str) -> Result<String, OpError> {
    let (parent, name) = split_rel(rel);
    let n = fsops::TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = format!(".{name}{}{}-{n}.{ext}", fsops::MARKER, std::process::id());
    remote_path(fs, root, &format!("{parent}/{temp}"))
}

/// Whether two states of a remote file match. Listings and single-file
/// lookups report times at different precision on some servers (S3: ms vs
/// s), so times are compared to the second; a missing time matches any.
fn same_state(a: Option<Snap>, b: Option<Snap>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.size == b.size
                && match (a.modified_ms, b.modified_ms) {
                    (Some(x), Some(y)) => x.div_euclid(1000) == y.div_euclid(1000),
                    _ => true,
                }
        }
        _ => false,
    }
}

fn stat_snap(fs: &dyn VfsWrite, path: &str) -> io::Result<Option<Snap>> {
    Ok(fs.stat(path)?.map(|m| Snap::from(&m)))
}

fn set_local_mtime(path: &Path, ms: i64) -> io::Result<()> {
    let t = FileTime::from_unix_time(ms.div_euclid(1000), (ms.rem_euclid(1000) * 1_000_000) as u32);
    filetime::set_file_mtime(path, t)
}

/// CRC-32 of everything a reader yields.
fn crc_of_reader(mut r: impl Read) -> io::Result<u32> {
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok(hasher.finalize()),
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Streams `src` (from `offset`) into the local `tmp`, which already holds
/// the first `offset` bytes. Returns the total size and, with `hash`, the
/// CRC-32 of the whole file.
fn receive(
    fs: &dyn VfsWrite,
    src: &str,
    tmp: &Path,
    offset: u64,
    hash: bool,
    on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
) -> Result<(u64, u32), OpError> {
    let mut hasher = crc32fast::Hasher::new();
    let mut out = if offset == 0 {
        fs::File::create_new(tmp).map_err(|e| OpError::io(tmp, e))?
    } else {
        if hash {
            let prefix = fs::File::open(tmp).map_err(|e| OpError::io(tmp, e))?;
            let mut buf = Vec::new();
            prefix
                .take(offset)
                .read_to_end(&mut buf)
                .map_err(|e| OpError::io(tmp, e))?;
            hasher.update(&buf);
        }
        fs::OpenOptions::new()
            .append(true)
            .open(tmp)
            .map_err(|e| OpError::io(tmp, e))?
    };
    let mut input = fs.open_at(src, offset)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = offset;
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        out.write_all(&buf[..n]).map_err(|e| OpError::io(tmp, e))?;
        if hash {
            hasher.update(&buf[..n]);
        }
        total += n as u64;
        on_bytes(n as u64)?;
    }
    out.sync_all().map_err(|e| OpError::io(tmp, e))?;
    Ok((total, hasher.finalize()))
}

/// Sends the local `src` file (from `offset`) to `target`.
fn push(
    fs: &dyn VfsWrite,
    src: &Path,
    target: &str,
    offset: u64,
    on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
) -> Result<u64, OpError> {
    let mut f = fs::File::open(src).map_err(|e| OpError::io(src, e))?;
    if offset > 0 {
        f.seek(SeekFrom::Start(offset)).map_err(|e| OpError::io(src, e))?;
    }
    Ok(fs.write(target, offset, &mut f, on_bytes)?)
}

/// Checks a finished upload: the source didn't change while it was sent,
/// the server holds every byte, and (with `verify`) reads back identically.
fn check_upload(fs: &dyn VfsWrite, src: &Path, sent: Snap, target: &str, verify: bool) -> Result<(), OpError> {
    if fsops::snap(src).map_err(|e| OpError::io(src, e))? != Some(sent) {
        return Err(OpError {
            path: Some(src.to_path_buf()),
            ..OpError::new(
                FailureKind::SourceChanged,
                "the source changed while it was being copied",
            )
        });
    }
    let got = fs.stat(target)?.map(|m| m.size);
    if got != Some(sent.size) {
        return Err(OpError::new(
            FailureKind::VerifyFailed,
            format!("the server holds {} of {} bytes", got.unwrap_or(0), sent.size),
        ));
    }
    if verify {
        let theirs = crc_of_reader(fs.open_at(target, 0)?)?;
        let ours = fsops::crc_of(src).map_err(|e| OpError::io(src, e))?;
        if theirs != ours {
            return Err(OpError::new(
                FailureKind::VerifyFailed,
                format!("the copy reads back with checksum {theirs:08x}, expected {ours:08x}"),
            ));
        }
    }
    Ok(())
}

impl<'a> Runner<'a> {
    /// Runs one operation where at least one side is remote. Paths come from
    /// [`SyncOp::dst_rel`] / [`SyncOp::src_rel`]: aligned pairs have a
    /// different name on the left (bc5:file-ops).
    pub(crate) fn run_once_remote(&mut self, op: &SyncOp, src: Root<'a>, dst: Root<'a>) -> Result<Done, OpError> {
        match (op.kind, dst) {
            (OpKind::Mkdir, Root::Local(root)) => {
                let d = fsops::resolve(root, op.dst_rel())?;
                if d.is_file() {
                    return Err(OpError::new(
                        FailureKind::TypeClash,
                        "a file with this name exists at the destination",
                    ));
                }
                fs::create_dir_all(&d).map_err(|e| OpError::io(&d, e))?;
                Ok(Done::default())
            }
            (OpKind::Mkdir, Root::Remote { fs, path, .. }) => {
                let d = remote_path(fs, path, op.dst_rel())?;
                if fs.stat(&d)?.is_some_and(|m| !m.is_dir) {
                    return Err(OpError::new(
                        FailureKind::TypeClash,
                        "a file with this name exists at the destination",
                    ));
                }
                fs.create_dir_all(&d)?;
                Ok(Done::default())
            }
            (OpKind::Delete, Root::Local(root)) => {
                let d = fsops::resolve(root, op.dst_rel())?;
                self.delete(op, &d)
            }
            (OpKind::Delete, Root::Remote { fs, path, .. }) => {
                let d = remote_path(fs, path, op.dst_rel())?;
                let Some(meta) = fs.stat(&d)? else {
                    return Ok(Done::default()); // already gone
                };
                self.check_remote_drift(op, &d, Some(Snap::from(&meta)))?;
                // No Recycle Bin on a remote: this is final.
                if meta.is_dir {
                    fs.remove_dir_all(&d)?;
                } else {
                    fs.remove_file(&d)?;
                }
                Ok(Done::default())
            }
            (OpKind::Copy, Root::Local(root)) => {
                let Root::Remote { fs, path, .. } = src else {
                    return Err(OpError::new(FailureKind::Invalid, "both sides are local"));
                };
                self.download(op, fs, path, root)
            }
            (OpKind::Copy, Root::Remote { fs, path, .. }) => self.upload(op, src, fs, path),
        }
    }

    /// [`Runner::check_drift`] with remote time precision.
    fn check_remote_drift(&self, op: &SyncOp, at: &str, now: Option<Snap>) -> Result<(), OpError> {
        let expected = match (op.kind, op.overwrite) {
            (OpKind::Copy, false) => None,
            _ => op.dst_before,
        };
        let now = if same_state(now, expected) { expected } else { now };
        self.check_drift(op, Path::new(at), now)
    }

    /// Remote → local: fetch into a temp sibling, then replace like a local copy.
    fn download(
        &mut self,
        op: &SyncOp,
        src_fs: &'a dyn VfsWrite,
        src_root: &str,
        dst_root: &Path,
    ) -> Result<Done, OpError> {
        let src = remote_path(src_fs, src_root, op.src_rel())?;
        let dst = fsops::resolve(dst_root, op.dst_rel())?;
        let now = fsops::snap(&dst).map_err(|e| OpError::io(&dst, e))?;
        if fs::symlink_metadata(&dst).is_ok_and(|m| m.is_dir()) {
            return Err(OpError::new(
                FailureKind::TypeClash,
                "a folder with this name exists at the destination",
            ));
        }
        self.check_drift(op, &dst, now)?;
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| OpError::io(parent, e))?;
        }
        let temp = self.fetch(op, src_fs, &src, &dst, 1)?;
        self.install(op, &dst, now, temp)
    }

    /// Downloads `src` into a temp sibling of the local path `near`,
    /// resuming a partial download of the same source if one was kept.
    /// Progress counts `1 / share` of the bytes.
    fn fetch(
        &mut self,
        op: &SyncOp,
        fs: &'a dyn VfsWrite,
        src: &str,
        near: &Path,
        share: u64,
    ) -> Result<TempCopy, OpError> {
        let before = fs
            .stat(src)?
            .ok_or_else(|| OpError::new(FailureKind::NotFound, "the source no longer exists"))?;
        if before.is_dir {
            return Err(OpError::new(FailureKind::TypeClash, "the source is a folder"));
        }
        let source = Snap::from(&before);
        let kept = self.partials.get_mut(&op.id).and_then(|p| p.download.take());
        let resume = match kept {
            Some((tmp, was)) if same_state(Some(was), Some(source)) && fs.caps().range_read => {
                match fs::metadata(&tmp) {
                    Ok(m) if m.len() <= source.size => Some((tmp, m.len())),
                    _ => {
                        let _ = fs::remove_file(&tmp);
                        None
                    }
                }
            }
            Some((tmp, _)) => {
                let _ = fs::remove_file(&tmp);
                None
            }
            None => None,
        };
        let (tmp, offset) = resume.unwrap_or_else(|| (fsops::temp_sibling(near), 0));

        let verify = self.opts.verify;
        let received = self.with_bytes(offset / share, share, |on_bytes| {
            receive(fs, src, &tmp, offset, verify, on_bytes)
        });
        let (bytes, crc) = match received {
            Ok(r) => r,
            Err(e) if e.kind == FailureKind::Network && tmp.exists() => {
                self.partials.entry(op.id).or_default().download = Some((tmp, source));
                return Err(e);
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        };
        let checked = (|| {
            let after = stat_snap(fs, src)?;
            if !same_state(after, Some(source)) || bytes != source.size {
                return Err(OpError::new(
                    FailureKind::SourceChanged,
                    "the source changed while it was being copied",
                ));
            }
            if verify {
                let check = fsops::crc_of(&tmp).map_err(|e| OpError::io(&tmp, e))?;
                if check != crc {
                    return Err(OpError::new(
                        FailureKind::VerifyFailed,
                        format!("the copy reads back with checksum {check:08x}, expected {crc:08x}"),
                    ));
                }
            }
            if let Some(ms) = source.modified_ms {
                set_local_mtime(&tmp, ms).map_err(|e| OpError::io(&tmp, e))?;
            }
            Ok(())
        })();
        if let Err(e) = checked {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(TempCopy { tmp, bytes, crc })
    }

    /// Local or remote → remote. A remote source is first downloaded to a
    /// local scratch file, so its consistency is settled before the target
    /// is touched.
    fn upload(&mut self, op: &SyncOp, src: Root<'a>, fs: &'a dyn VfsWrite, root: &str) -> Result<Done, OpError> {
        let d = remote_path(fs, root, op.dst_rel())?;
        let now = match fs.stat(&d)? {
            Some(m) if m.is_dir => {
                return Err(OpError::new(
                    FailureKind::TypeClash,
                    "a folder with this name exists at the destination",
                ))
            }
            m => m.map(|m| Snap::from(&m)),
        };
        self.check_remote_drift(op, &d, now)?;
        match src {
            Root::Local(src_root) => {
                let s = fsops::resolve(src_root, op.src_rel())?;
                self.send(op, &s, fs, root, &d, now.is_some(), 0, 1)
            }
            Root::Remote {
                fs: src_fs,
                path: src_root,
                ..
            } => {
                let s = remote_path(src_fs, src_root, op.src_rel())?;
                let scratch = std::env::temp_dir().join("pcompare-sync");
                fs::create_dir_all(&scratch).map_err(|e| OpError::io(&scratch, e))?;
                let near = scratch.join(split_rel(op.src_rel()).1);
                let temp = self.fetch(op, src_fs, &s, &near, 2)?;
                let sent = self.send(op, &temp.tmp, fs, root, &d, now.is_some(), temp.bytes / 2, 2);
                let _ = fs::remove_file(&temp.tmp);
                sent
            }
        }
    }

    /// Sends the local file `src` to `d` on `fs`, replacing what is there if
    /// `replace`. Progress starts at `already` and counts `1 / share`.
    #[allow(clippy::too_many_arguments)]
    fn send(
        &mut self,
        op: &SyncOp,
        src: &Path,
        fs: &'a dyn VfsWrite,
        root: &str,
        d: &str,
        replace: bool,
        already: u64,
        share: u64,
    ) -> Result<Done, OpError> {
        let caps = fs.caps();
        let meta = fs::metadata(src).map_err(|e| OpError::io(src, e))?;
        if meta.is_dir() {
            return Err(OpError::new(FailureKind::TypeClash, "the source is a folder"));
        }
        let source = fsops::snap_of(&meta);
        let (parent, _) = split_rel(op.dst_rel());
        if caps.folders && !parent.is_empty() {
            fs.create_dir_all(&remote_path(fs, root, parent)?)?;
        }
        let verify = self.opts.verify;
        let mut note = None;

        if !caps.rename {
            // The write commits all at once (object stores; OpenDAL's FTP
            // writes via a temp name itself): a failure leaves the old file.
            self.with_bytes(already, share, |on_bytes| push(fs, src, d, 0, on_bytes))?;
            check_upload(fs, src, source, d, verify)?;
        } else {
            let kept = self.partials.get_mut(&op.id).and_then(|p| p.upload.take());
            let (tmp, offset) = match kept {
                Some((_, tmp, was)) if was == source && caps.resume_write => match fs.stat(&tmp) {
                    Ok(Some(m)) if !m.is_dir && m.size <= source.size => (tmp, m.size),
                    _ => (tmp, 0),
                },
                Some((_, tmp, _)) => {
                    let _ = fs.remove_file(&tmp);
                    (temp_path(fs, root, op.dst_rel(), "tmp")?, 0)
                }
                None => (temp_path(fs, root, op.dst_rel(), "tmp")?, 0),
            };
            let sent = self.with_bytes(already + offset / share, share, |on_bytes| {
                push(fs, src, &tmp, offset, on_bytes)
            });
            if let Err(e) = sent {
                if e.kind == FailureKind::Network && caps.resume_write {
                    self.partials.entry(op.id).or_default().upload = Some((fs, tmp, source));
                } else {
                    let _ = fs.remove_file(&tmp);
                }
                return Err(e);
            }
            if let Err(e) = check_upload(fs, src, source, &tmp, verify) {
                let _ = fs.remove_file(&tmp);
                return Err(e);
            }
            if let Some(ms) = source.modified_ms.filter(|_| caps.set_modified) {
                if fs.set_modified(&tmp, ms).is_err() {
                    note = Some("copied, but the server didn't accept the file's modification time".to_owned());
                }
            }
            if !replace {
                if let Err(e) = fs.rename(&tmp, d) {
                    let _ = fs.remove_file(&tmp);
                    return Err(e.into());
                }
            } else {
                // Move the old version aside (journaled), swap the new one in,
                // then delete the old one — remotes have no Recycle Bin.
                let staged = temp_path(fs, root, op.dst_rel(), "old")?;
                if let Err(e) = fs.rename(d, &staged) {
                    let _ = fs.remove_file(&tmp);
                    return Err(e.into());
                }
                self.journal.backup_staged(op.id, Path::new(&staged));
                if let Err(e) = fs.rename(&tmp, d) {
                    let _ = fs.remove_file(&tmp);
                    return Err(match fs.rename(&staged, d) {
                        Ok(()) => e.into(),
                        Err(e2) => OpError::new(
                            FailureKind::Other,
                            format!("{e}; restoring the old version also failed ({e2}); it is at {staged}"),
                        ),
                    });
                }
                if let Err(e) = fs.remove_file(&staged) {
                    note = Some(format!(
                        "copied, but the old version couldn't be removed ({e}); it was kept as {staged}"
                    ));
                }
            }
        }
        Ok(Done {
            bytes: source.size,
            backup: None,
            written: stat_snap(fs, d).ok().flatten(),
            note,
        })
    }

    /// Deletes whatever partial transfers `id` left behind.
    pub(crate) fn discard_partial(&mut self, id: u32) {
        if let Some(p) = self.partials.remove(&id) {
            if let Some((tmp, _)) = p.download {
                let _ = fs::remove_file(tmp);
            }
            if let Some((fs, tmp, _)) = p.upload {
                let _ = fs.remove_file(&tmp);
            }
        }
    }
}

// ---------- plan warnings and pre-flight ----------

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// Warns that a plan deletes or replaces files on a remote side, where no
/// Recycle Bin can bring them back and a rollback can't either.
pub fn remote_warnings(plan: &SyncPlan, left_remote: bool, right_remote: bool) -> Vec<PlanWarning> {
    let mut out = Vec::new();
    for (remote, dir, name) in [
        (left_remote, Direction::ToLeft, "left"),
        (right_remote, Direction::ToRight, "right"),
    ] {
        if !remote {
            continue;
        }
        let ops = plan.ops.iter().filter(|o| o.direction == dir);
        let deletes = ops.clone().filter(|o| o.kind == OpKind::Delete).count();
        let replaces = ops.filter(|o| o.kind == OpKind::Copy && o.overwrite).count();
        let what = match (deletes, replaces) {
            (0, 0) => continue,
            (d, 0) => count(d, "deletion", "deletions"),
            (0, r) => count(r, "replaced file", "replaced files"),
            (d, r) => format!(
                "{} and {}",
                count(d, "deletion", "deletions"),
                count(r, "replaced file", "replaced files")
            ),
        };
        out.push(PlanWarning {
            kind: WarningKind::RemotePermanent,
            message: format!(
                "The {name} folder is remote: its {what} are permanent. Remote folders have no Recycle Bin, \
                 so deleted and overwritten files there can't be restored or rolled back."
            ),
        });
    }
    out
}

fn reachable(root: Root) -> bool {
    match root {
        Root::Local(p) => p.is_dir(),
        Root::Remote { fs, path, .. } => fs.read_dir(path).is_ok(),
    }
}

/// [`exec::preflight`] where either side may be remote: both folders are
/// reachable, neither contains the other, sides about to be written to
/// accept writes, local sides have room, and remote deletions are flagged.
pub fn preflight_on(left: Root, right: Root, ops: &[SyncOp]) -> Vec<PreflightIssue> {
    if let (Root::Local(l), Root::Local(r)) = (left, right) {
        return exec::preflight(l, r, ops);
    }
    let mut issues = Vec::new();
    let block = |issues: &mut Vec<PreflightIssue>, message: String| {
        issues.push(PreflightIssue {
            blocking: true,
            message,
        })
    };
    for (name, root) in [("left", left), ("right", right)] {
        if !reachable(root) {
            block(
                &mut issues,
                format!("The {name} folder {} doesn't exist or isn't reachable.", root.label()),
            );
        }
    }
    if !issues.is_empty() {
        return issues;
    }
    if let (Root::Remote { label: a, .. }, Root::Remote { label: b, .. }) = (left, right) {
        let (a, b) = (
            format!("{}/", a.trim_end_matches('/')),
            format!("{}/", b.trim_end_matches('/')),
        );
        if a.eq_ignore_ascii_case(&b) {
            block(&mut issues, "Both sides are the same folder.".into());
        } else if a.starts_with(&b) || b.starts_with(&a) {
            block(
                &mut issues,
                "One folder is inside the other; syncing them would copy the folder into itself.".into(),
            );
        }
    }

    for (name, root, dir) in [("left", left, Direction::ToLeft), ("right", right, Direction::ToRight)] {
        let incoming: Vec<&SyncOp> = ops.iter().filter(|o| o.direction == dir).collect();
        if incoming.is_empty() {
            continue;
        }
        match root {
            Root::Local(p) => exec::local_side_issues(name, p, &incoming, &mut issues),
            Root::Remote { fs, path, label } => {
                let probe = fs.join(path, &format!(".pcompare-probe-{}", std::process::id()));
                match fs.write(&probe, 0, &mut &b"pcompare"[..], &mut |_| Ok(())) {
                    Ok(_) => {
                        let _ = fs.remove_file(&probe);
                    }
                    Err(e) => block(&mut issues, format!("Can't write to the {name} folder {label}: {e}.")),
                }
                let permanent = incoming
                    .iter()
                    .filter(|o| o.kind == OpKind::Delete || (o.kind == OpKind::Copy && o.overwrite))
                    .count();
                if permanent > 0 {
                    issues.push(PreflightIssue {
                        blocking: false,
                        message: format!(
                            "{} on the {name} side ({label}) can't be undone: remote folders have no Recycle Bin.",
                            count(permanent, "deletion or overwrite", "deletions and overwrites")
                        ),
                    });
                }
            }
        }
    }
    issues
}

// ---------- recovery and rollback ----------

/// Removes temp files a crashed transfer left next to `rel` on a remote.
fn remove_stale_remote_temps(fs: &dyn VfsWrite, root: &str, rel: &str) {
    let (parent, name) = split_rel(rel);
    let dir = if parent.is_empty() {
        root.to_owned()
    } else {
        match remote_path(fs, root, parent) {
            Ok(d) => d,
            Err(_) => return,
        }
    };
    let prefix = format!(".{name}{}", fsops::MARKER);
    let Ok(entries) = fs.read_dir(&dir) else {
        return;
    };
    for e in entries {
        if !e.meta.is_dir && e.name.starts_with(&prefix) && e.name.ends_with(".tmp") {
            let _ = fs.remove_file(&fs.join(&dir, &e.name));
        }
    }
}

/// [`exec::recover_op`] where either side may be remote. `staged` is the
/// journaled place the old destination was moved to, in its side's syntax.
pub fn recover_op_on(left: Root, right: Root, op: &SyncOp, staged: Option<&str>, deleter: &dyn Deleter) -> Recovery {
    let root = match op.direction {
        Direction::ToRight => right,
        Direction::ToLeft => left,
    };
    let (fs, root) = match root {
        Root::Local(p) => return exec::recover_op(p, p, op, staged.map(Path::new), deleter),
        Root::Remote { fs, path, .. } => (fs, path),
    };
    let Ok(dst) = remote_path(fs, root, op.dst_rel()) else {
        return Recovery::Clean;
    };
    remove_stale_remote_temps(fs, root, op.dst_rel());
    let Some(staged) = staged.filter(|s| matches!(fs.stat(s), Ok(Some(_)))) else {
        return Recovery::Clean;
    };
    let manual = |e: io::Error| Recovery::Manual {
        message: format!("{}: the previous version is at {staged} ({e})", op.rel_path),
    };
    match fs.stat(&dst) {
        Ok(None) => match fs.rename(staged, &dst) {
            Ok(()) => Recovery::Restored,
            Err(e) => manual(e),
        },
        // The new file made it; the old one has nowhere to go but away.
        Ok(Some(_)) => match fs.remove_file(staged) {
            Ok(()) => Recovery::Completed { backup: None },
            Err(e) => manual(e),
        },
        Err(e) => manual(e),
    }
}

/// Undoes one operation on a remote side, if that is possible at all.
fn undo_remote(fs: &dyn VfsWrite, root: &str, it: &RollbackItem) -> Result<(), String> {
    const GONE: &str = "remote folders have no Recycle Bin, so the previous version was removed for good";
    let op = &it.op;
    let dst = remote_path(fs, root, op.dst_rel()).map_err(|e| e.message)?;
    match op.kind {
        OpKind::Mkdir => match fs.stat(&dst).map_err(|e| e.to_string())? {
            None => Ok(()),
            Some(_) if matches!(fs.read_dir(&dst), Ok(e) if e.is_empty()) => {
                fs.remove_dir_all(&dst).map_err(|e| e.to_string())
            }
            Some(_) => Err("kept: the folder isn't empty".into()),
        },
        OpKind::Delete => Err(format!("can't restore a remote deletion: {GONE}")),
        OpKind::Copy if op.overwrite => Err(format!("can't restore the replaced remote file: {GONE}")),
        OpKind::Copy => {
            let now = stat_snap(fs, &dst).map_err(|e| e.to_string())?;
            match now {
                None => Ok(()),
                Some(_) if !same_state(now, it.result.written) => {
                    Err("kept: the file was modified after the sync".into())
                }
                // The copy still exists on the source side, so this loses nothing.
                Some(_) => fs.remove_file(&dst).map_err(|e| e.to_string()),
            }
        }
    }
}

/// [`exec::rollback`] where either side may be remote. Remote deletions and
/// overwrites are reported as impossible to undo; files and folders the
/// sync created remotely are removed.
pub fn rollback_on(left: Root, right: Root, items: &[RollbackItem], deleter: &dyn Deleter) -> Vec<RollbackResult> {
    items
        .iter()
        .rev()
        .filter(|it| it.result.ok)
        .map(|it| {
            let root = match it.op.direction {
                Direction::ToRight => right,
                Direction::ToLeft => left,
            };
            let outcome = match root {
                Root::Local(p) => exec::undo_one(p, p, it, deleter),
                Root::Remote { fs, path, .. } => undo_remote(fs, path, it),
            };
            RollbackResult {
                id: it.op.id,
                rel_path: it.op.rel_path.clone(),
                ok: outcome.is_ok(),
                message: outcome.err(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{execute_on, ExecOptions, ExecReport};
    use crate::fsops::{PermanentDeleter, VersioningDeleter};
    use crate::journal::{Journal, MemoryJournal, NullJournal};
    use crate::sync::{plan_sync_local, PlanOptions, SyncMode};
    use pcompare_core::folder::{EntryMeta, FolderCompareOptions};
    use pcompare_core::vfs::{LocalFs, Vfs, VfsEntry};
    use pcompare_core::vfs_write::WriteCaps;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    const OLD: i64 = 1_600_000_000;
    const NEW: i64 = 1_700_000_000;

    const FULL: WriteCaps = WriteCaps {
        rename: true,
        set_modified: true,
        resume_write: true,
        range_read: true,
        folders: true,
    };
    /// Like S3: no rename, no timestamps, folders implied.
    const OBJECT_STORE: WriteCaps = WriteCaps {
        rename: false,
        set_modified: false,
        resume_write: false,
        range_read: true,
        folders: false,
    };

    /// A local folder posing as a remote: chosen capabilities, timestamps to
    /// the second only, and connections that drop part-way on request.
    struct FakeRemote {
        caps: WriteCaps,
        /// Transfers that fail with a connection reset after `drop_after` bytes.
        drops: Mutex<u32>,
        drop_after: usize,
        read_offsets: Mutex<Vec<u64>>,
        write_offsets: Mutex<Vec<u64>>,
    }

    impl FakeRemote {
        fn new(caps: WriteCaps) -> Self {
            FakeRemote {
                caps,
                drops: Mutex::new(0),
                drop_after: 0,
                read_offsets: Mutex::default(),
                write_offsets: Mutex::default(),
            }
        }

        fn flaky(caps: WriteCaps, drops: u32, drop_after: usize) -> Self {
            FakeRemote {
                drops: Mutex::new(drops),
                drop_after,
                ..Self::new(caps)
            }
        }

        fn take_drop(&self) -> bool {
            let mut left = self.drops.lock().unwrap();
            let drop = *left > 0;
            *left = left.saturating_sub(1);
            drop
        }
    }

    fn coarse(mut m: EntryMeta) -> EntryMeta {
        m.modified_ms = m.modified_ms.map(|ms| ms.div_euclid(1000) * 1000);
        m
    }

    fn reset() -> io::Error {
        io::Error::new(io::ErrorKind::ConnectionReset, "connection reset by peer")
    }

    /// Yields `left` bytes, then fails like a dropped connection.
    struct Dropping<R> {
        inner: R,
        left: usize,
    }

    impl<R: Read> Read for Dropping<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.left == 0 {
                return Err(reset());
            }
            let n = buf.len().min(self.left);
            let n = self.inner.read(&mut buf[..n])?;
            self.left -= n;
            Ok(n)
        }
    }

    impl Vfs for FakeRemote {
        fn read_dir(&self, dir: &str) -> pcompare_core::Result<Vec<VfsEntry>> {
            let mut entries = LocalFs.read_dir(dir)?;
            for e in &mut entries {
                e.meta = coarse(e.meta.clone());
            }
            Ok(entries)
        }
        fn open(&self, path: &str) -> pcompare_core::Result<Box<dyn Read + Send>> {
            LocalFs.open(path)
        }
        fn join(&self, dir: &str, name: &str) -> String {
            LocalFs.join(dir, name)
        }
    }

    impl VfsWrite for FakeRemote {
        fn caps(&self) -> WriteCaps {
            self.caps
        }
        fn stat(&self, path: &str) -> io::Result<Option<EntryMeta>> {
            Ok(LocalFs.stat(path)?.map(coarse))
        }
        fn open_at(&self, path: &str, offset: u64) -> io::Result<Box<dyn Read + Send>> {
            self.read_offsets.lock().unwrap().push(offset);
            let inner = LocalFs.open_at(path, offset)?;
            if self.take_drop() {
                return Ok(Box::new(Dropping {
                    inner,
                    left: self.drop_after,
                }));
            }
            Ok(inner)
        }
        fn write(
            &self,
            path: &str,
            offset: u64,
            src: &mut dyn Read,
            on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
        ) -> io::Result<u64> {
            self.write_offsets.lock().unwrap().push(offset);
            if self.take_drop() {
                LocalFs.write(path, offset, &mut src.take(self.drop_after as u64), on_bytes)?;
                return Err(reset());
            }
            LocalFs.write(path, offset, src, on_bytes)
        }
        fn create_dir_all(&self, path: &str) -> io::Result<()> {
            LocalFs.create_dir_all(path)
        }
        fn remove_file(&self, path: &str) -> io::Result<()> {
            LocalFs.remove_file(path)
        }
        fn remove_dir_all(&self, path: &str) -> io::Result<()> {
            LocalFs.remove_dir_all(path)
        }
        fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            if !self.caps.rename {
                return Err(io::Error::from(io::ErrorKind::Unsupported));
            }
            LocalFs.rename(from, to)
        }
        fn set_modified(&self, path: &str, ms: i64) -> io::Result<()> {
            if !self.caps.set_modified {
                return Err(io::Error::from(io::ErrorKind::Unsupported));
            }
            LocalFs.set_modified(path, ms)
        }
    }

    fn write(root: &Path, rel: &str, body: &str, secs: i64, nanos: u32) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        filetime::set_file_mtime(&p, FileTime::from_unix_time(secs, nanos)).unwrap();
    }

    fn read(root: &Path, rel: &str) -> String {
        fs::read_to_string(root.join(rel)).unwrap()
    }

    fn mtime_secs(root: &Path, rel: &str) -> i64 {
        FileTime::from_last_modification_time(&fs::metadata(root.join(rel)).unwrap()).unix_seconds()
    }

    /// Every file under `root`, so leftover temp files show up.
    fn all_files(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut dirs = vec![root.to_path_buf()];
        while let Some(d) = dirs.pop() {
            for e in fs::read_dir(&d).unwrap().flatten() {
                if e.path().is_dir() {
                    dirs.push(e.path());
                } else {
                    out.push(
                        e.path()
                            .strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        out.sort();
        out
    }

    struct Pair {
        dir: tempfile::TempDir,
        left: PathBuf,
        right: PathBuf,
        l: String,
        r: String,
    }

    /// Both sides hold a.txt, b.txt and docs/c.txt, in sync; the right a.txt
    /// has a sub-second timestamp (which remote listings round off).
    fn synced() -> Pair {
        let dir = tempfile::tempdir().unwrap();
        let (left, right) = (dir.path().join("L"), dir.path().join("R"));
        for root in [&left, &right] {
            write(root, "a.txt", "a1", OLD, 0);
            write(root, "b.txt", "b1", OLD, 0);
            write(root, "docs/c.txt", "c1", OLD, 0);
        }
        write(&right, "a.txt", "a1", OLD, 0);
        let (l, r) = (
            left.to_string_lossy().into_owned(),
            right.to_string_lossy().into_owned(),
        );
        Pair { dir, left, right, l, r }
    }

    fn fast() -> ExecOptions {
        ExecOptions {
            retry_delay_ms: 1,
            retry_max_delay_ms: 2,
            verify: true,
            ..Default::default()
        }
    }

    fn mirror(p: &Pair) -> SyncPlan {
        let opts = FolderCompareOptions {
            ignore_name_case: false,
            ..Default::default()
        };
        plan_sync_local(
            &p.left,
            &p.right,
            SyncMode::MirrorLeftToRight,
            &opts,
            &PlanOptions::default(),
        )
        .unwrap()
    }

    fn run(left: Root, right: Root, plan: &SyncPlan, deleter: &dyn Deleter, journal: &mut dyn Journal) -> ExecReport {
        execute_on(
            left,
            right,
            &plan.ops,
            &fast(),
            deleter,
            journal,
            &AtomicBool::new(false),
            &mut |_| {},
        )
    }

    /// Edits a.txt, deletes b.txt and adds fresh/new.txt on the left.
    fn edit_left(p: &Pair) {
        write(&p.left, "a.txt", "a2 longer", NEW, 0);
        fs::remove_file(p.left.join("b.txt")).unwrap();
        write(&p.left, "fresh/new.txt", "n", OLD, 0);
    }

    #[test]
    fn upload_swaps_new_files_into_place() {
        let p = synced();
        write(&p.right, "a.txt", "a1", OLD, 500_000_000);
        edit_left(&p);
        let plan = mirror(&p);
        let remote = FakeRemote::new(FULL);
        let mut journal = MemoryJournal::default();
        let r = run(
            Root::Local(&p.left),
            Root::Remote {
                fs: &remote,
                path: &p.r,
                label: "remote://box/R",
            },
            &plan,
            &PermanentDeleter,
            &mut journal,
        );
        assert_eq!(r.failed, 0, "{:?}", r.results);
        assert_eq!(read(&p.right, "a.txt"), "a2 longer");
        assert_eq!(read(&p.right, "fresh/new.txt"), "n");
        assert!(!p.right.join("b.txt").exists());
        assert_eq!(mtime_secs(&p.right, "a.txt"), NEW, "timestamp carried over");
        assert_eq!(journal.staged.len(), 1, "the replaced file was staged and journaled");
        assert_eq!(
            all_files(&p.right),
            ["a.txt", "docs/c.txt", "fresh/new.txt"],
            "no temp files left"
        );
    }

    #[test]
    fn object_stores_get_direct_writes() {
        let p = synced();
        edit_left(&p);
        let plan = mirror(&p);
        let remote = FakeRemote::new(OBJECT_STORE);
        let r = run(
            Root::Local(&p.left),
            Root::Remote {
                fs: &remote,
                path: &p.r,
                label: "remote://s3/R",
            },
            &plan,
            &PermanentDeleter,
            &mut NullJournal,
        );
        assert_eq!(r.failed, 0, "{:?}", r.results);
        assert_eq!(read(&p.right, "a.txt"), "a2 longer");
        assert_eq!(all_files(&p.right), ["a.txt", "docs/c.txt", "fresh/new.txt"]);
    }

    #[test]
    fn download_keeps_old_versions_locally() {
        let p = synced();
        edit_left(&p);
        let plan = mirror(&p);
        let remote = FakeRemote::new(OBJECT_STORE);
        let vault = p.dir.path().join("versions");
        let deleter = VersioningDeleter::new(&vault, "run1");
        let r = run(
            Root::Remote {
                fs: &remote,
                path: &p.l,
                label: "remote://s3/L",
            },
            Root::Local(&p.right),
            &plan,
            &deleter,
            &mut NullJournal,
        );
        assert_eq!(r.failed, 0, "{:?}", r.results);
        assert_eq!(read(&p.right, "a.txt"), "a2 longer");
        assert_eq!(mtime_secs(&p.right, "a.txt"), NEW);
        assert_eq!(read(&vault, "run1/right/a.txt"), "a1", "replaced version kept");
        assert_eq!(read(&vault, "run1/right/b.txt"), "b1", "deleted file kept");
    }

    #[test]
    fn remote_to_remote_goes_through_a_scratch_file() {
        let p = synced();
        edit_left(&p);
        let plan = mirror(&p);
        let (src, dst) = (FakeRemote::new(OBJECT_STORE), FakeRemote::new(FULL));
        let r = run(
            Root::Remote {
                fs: &src,
                path: &p.l,
                label: "remote://s3/L",
            },
            Root::Remote {
                fs: &dst,
                path: &p.r,
                label: "remote://box/R",
            },
            &plan,
            &PermanentDeleter,
            &mut NullJournal,
        );
        assert_eq!(r.failed, 0, "{:?}", r.results);
        assert_eq!(r.bytes_copied, 10);
        assert_eq!(read(&p.right, "a.txt"), "a2 longer");
        assert_eq!(read(&p.right, "fresh/new.txt"), "n");
        assert_eq!(all_files(&p.right), ["a.txt", "docs/c.txt", "fresh/new.txt"]);
    }

    fn one_new_file() -> (Pair, SyncPlan) {
        let p = synced();
        write(&p.left, "big.bin", "0123456789", OLD, 0);
        let plan = mirror(&p);
        assert_eq!(plan.ops.len(), 1);
        (p, plan)
    }

    #[test]
    fn dropped_download_resumes_where_it_stopped() {
        let (p, plan) = one_new_file();
        let remote = FakeRemote::flaky(OBJECT_STORE, 1, 4);
        let r = run(
            Root::Remote {
                fs: &remote,
                path: &p.l,
                label: "remote://s3/L",
            },
            Root::Local(&p.right),
            &plan,
            &PermanentDeleter,
            &mut NullJournal,
        );
        assert_eq!(r.failed, 0, "{:?}", r.results);
        assert_eq!(r.results[0].attempts, 2);
        assert_eq!(read(&p.right, "big.bin"), "0123456789");
        assert_eq!(
            *remote.read_offsets.lock().unwrap(),
            [0, 4],
            "second attempt resumed at byte 4"
        );
        assert_eq!(all_files(&p.right), ["a.txt", "b.txt", "big.bin", "docs/c.txt"]);
    }

    #[test]
    fn dropped_upload_resumes_where_the_server_allows() {
        let (p, plan) = one_new_file();
        let remote = FakeRemote::flaky(FULL, 1, 4);
        let r = run(
            Root::Local(&p.left),
            Root::Remote {
                fs: &remote,
                path: &p.r,
                label: "remote://box/R",
            },
            &plan,
            &PermanentDeleter,
            &mut NullJournal,
        );
        assert_eq!(r.failed, 0, "{:?}", r.results);
        assert_eq!(read(&p.right, "big.bin"), "0123456789");
        assert_eq!(*remote.write_offsets.lock().unwrap(), [0, 4]);

        // Without offset writes the upload starts over, and nothing is left behind.
        let (p, plan) = one_new_file();
        let caps = WriteCaps {
            resume_write: false,
            ..FULL
        };
        let remote = FakeRemote::flaky(caps, 1, 4);
        let r = run(
            Root::Local(&p.left),
            Root::Remote {
                fs: &remote,
                path: &p.r,
                label: "remote://box/R",
            },
            &plan,
            &PermanentDeleter,
            &mut NullJournal,
        );
        assert_eq!(r.failed, 0, "{:?}", r.results);
        assert_eq!(*remote.write_offsets.lock().unwrap(), [0, 0]);
        assert_eq!(all_files(&p.right), ["a.txt", "b.txt", "big.bin", "docs/c.txt"]);
    }

    #[test]
    fn remote_target_changed_since_preview_is_left_alone() {
        let p = synced();
        edit_left(&p);
        let plan = mirror(&p);
        write(&p.right, "a.txt", "someone else's edit", NEW + 5, 0);
        let remote = FakeRemote::new(FULL);
        let r = run(
            Root::Local(&p.left),
            Root::Remote {
                fs: &remote,
                path: &p.r,
                label: "remote://box/R",
            },
            &plan,
            &PermanentDeleter,
            &mut NullJournal,
        );
        let a = r.results.iter().find(|x| x.rel_path == "a.txt").unwrap();
        assert_eq!(a.failure, Some(FailureKind::ChangedSincePreview));
        assert_eq!(read(&p.right, "a.txt"), "someone else's edit");
    }

    #[test]
    fn rollback_removes_created_files_and_refuses_permanent_changes() {
        let p = synced();
        edit_left(&p);
        let plan = mirror(&p);
        let remote = FakeRemote::new(FULL);
        let right = Root::Remote {
            fs: &remote,
            path: &p.r,
            label: "remote://box/R",
        };
        let r = run(Root::Local(&p.left), right, &plan, &PermanentDeleter, &mut NullJournal);
        assert_eq!(r.failed, 0);
        let items: Vec<_> = plan
            .ops
            .iter()
            .zip(&r.results)
            .map(|(op, res)| RollbackItem {
                op: op.clone(),
                result: res.clone(),
            })
            .collect();
        let undo = rollback_on(Root::Local(&p.left), right, &items, &PermanentDeleter);
        let by_path = |rel: &str| undo.iter().find(|u| u.rel_path == rel).unwrap();
        assert!(by_path("fresh/new.txt").ok && by_path("fresh").ok, "{undo:?}");
        assert!(!p.right.join("fresh").exists(), "created file and folder removed");
        for rel in ["a.txt", "b.txt"] {
            let u = by_path(rel);
            assert!(!u.ok && u.message.as_deref().unwrap().contains("Recycle Bin"), "{u:?}");
        }
    }

    #[test]
    fn recovery_puts_a_staged_remote_file_back() {
        let p = synced();
        let remote = FakeRemote::new(FULL);
        let right = Root::Remote {
            fs: &remote,
            path: &p.r,
            label: "remote://box/R",
        };
        let op = SyncOp {
            id: 0,
            kind: OpKind::Copy,
            direction: Direction::ToRight,
            rel_path: "a.txt".into(),
            is_dir: false,
            size: 2,
            overwrite: true,
            reason: String::new(),
            dst_before: None,
            left_rel_path: None,
            category: None,
        };
        // Crash after staging, with a half-sent temp file lying around.
        let staged = temp_path(&remote, &p.r, "a.txt", "old").unwrap();
        fs::rename(p.right.join("a.txt"), &staged).unwrap();
        fs::write(temp_path(&remote, &p.r, "a.txt", "tmp").unwrap(), "half").unwrap();
        let rec = recover_op_on(Root::Local(&p.left), right, &op, Some(&staged), &PermanentDeleter);
        assert_eq!(rec, Recovery::Restored);
        assert_eq!(all_files(&p.right), ["a.txt", "b.txt", "docs/c.txt"]);
    }

    #[test]
    fn plan_and_preflight_flag_permanent_remote_changes() {
        let p = synced();
        edit_left(&p);
        let plan = mirror(&p);
        let w = remote_warnings(&plan, false, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].kind, WarningKind::RemotePermanent);
        assert!(
            w[0].message.contains("1 deletion and 1 replaced file"),
            "{}",
            w[0].message
        );
        assert!(
            remote_warnings(&plan, true, false).is_empty(),
            "nothing is deleted on the left"
        );

        let remote = FakeRemote::new(FULL);
        let right = Root::Remote {
            fs: &remote,
            path: &p.r,
            label: "remote://box/R",
        };
        let issues = preflight_on(Root::Local(&p.left), right, &plan.ops);
        assert!(issues.iter().all(|i| !i.blocking), "{issues:?}");
        assert!(
            issues.iter().any(|i| i.message.contains("can't be undone")),
            "{issues:?}"
        );
        assert_eq!(all_files(&p.right), ["a.txt", "b.txt", "docs/c.txt"], "probe removed");

        let missing = p.dir.path().join("gone").to_string_lossy().into_owned();
        let gone = Root::Remote {
            fs: &remote,
            path: &missing,
            label: "remote://box/gone",
        };
        assert!(preflight_on(Root::Local(&p.left), gone, &plan.ops)
            .iter()
            .any(|i| i.blocking));
    }
}
