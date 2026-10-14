//! Object stores and cloud drives through Apache OpenDAL.

use crate::profile::ResolvedProfile;
use crate::provider::{spec, FieldKind, ProviderKind};
use crate::{rt, NetRead};
use pcompare_core::folder::EntryMeta;
use pcompare_core::vfs::{Vfs, VfsEntry};
use pcompare_core::vfs_write::{unsupported, VfsWrite, WriteCaps};
use pcompare_core::{CoreError, Result};
use std::io::{self, Read};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct OpendalFs {
    op: opendal::Operator,
    blocking: opendal::blocking::Operator,
    kind: ProviderKind,
}

fn scheme(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::S3 => "s3",
        ProviderKind::Azblob => "azblob",
        ProviderKind::Gcs => "gcs",
        ProviderKind::B2 => "b2",
        ProviderKind::Ftp => "ftp",
        ProviderKind::Webdav => "webdav",
        ProviderKind::Onedrive => "onedrive",
        ProviderKind::Dropbox => "dropbox",
        ProviderKind::Gdrive => "gdrive",
        ProviderKind::Sftp => unreachable!("SFTP uses the russh backend"),
        ProviderKind::Sharepoint => unreachable!("SharePoint uses the Graph backend"),
    }
}

/// Translates profile fields into the service's OpenDAL config map.
fn config(p: &ResolvedProfile, access_token: Option<&str>) -> Vec<(String, String)> {
    let mut cfg: Vec<(String, String)> = Vec::new();
    let mut put = |k: &str, v: &str| cfg.push((k.to_owned(), v.to_owned()));
    if let Some(root) = p.get("root") {
        put("root", root);
    }
    match p.profile.kind {
        ProviderKind::Ftp => {
            let secure = p.get("secure").is_none_or(|v| v == "true");
            let host = p.get("host").unwrap_or_default();
            let port = p.get("port").unwrap_or("21");
            put(
                "endpoint",
                &format!("{}://{host}:{port}", if secure { "ftps" } else { "ftp" }),
            );
            put("user", p.get("user").unwrap_or("anonymous"));
            if let Some(pw) = p.get("password") {
                put("password", pw);
            }
        }
        ProviderKind::Onedrive | ProviderKind::Dropbox | ProviderKind::Gdrive => {
            // We refresh tokens ourselves (see manager.rs) and hand OpenDAL a
            // short-lived access token; this keeps PKCE public clients working.
            put("access_token", access_token.unwrap_or_default());
        }
        kind => {
            // Field keys for these services are OpenDAL's own config keys.
            for f in spec(kind).fields {
                if f.key == "root" || f.kind == FieldKind::Checkbox {
                    continue;
                }
                if let Some(v) = p.get(f.key) {
                    put(f.key, v);
                }
            }
            if kind == ProviderKind::S3 {
                if p.get("region").is_none() {
                    put("region", "us-east-1");
                }
                if p.get("access_key_id").is_some() {
                    // Don't let ~/.aws or env credentials override the profile.
                    put("disable_config_load", "true");
                }
            }
        }
    }
    cfg
}

fn remote_err(path: &str, e: opendal::Error) -> CoreError {
    CoreError::remote(path, e)
}

impl OpendalFs {
    pub fn connect(p: &ResolvedProfile, access_token: Option<&str>) -> Result<Self> {
        opendal::install_default();
        let op = opendal::Operator::via_iter(scheme(p.profile.kind), config(p, access_token))
            .map_err(|e| CoreError::remote(&p.profile.name, e))?
            .layer(opendal::layers::RetryLayer::new().with_max_times(3))
            .layer(opendal::layers::TimeoutLayer::new());
        let blocking = {
            let _guard = rt().enter();
            opendal::blocking::Operator::new(op.clone()).map_err(|e| CoreError::remote(&p.profile.name, e))?
        };
        Ok(Self {
            op,
            blocking,
            kind: p.profile.kind,
        })
    }
}

/// OpenDAL wants `""`/`"/"` for the root, `"a/b/"` (trailing slash) for dirs.
fn dir_path(dir: &str) -> String {
    let d = dir.trim_matches('/');
    if d.is_empty() {
        "/".into()
    } else {
        format!("{d}/")
    }
}

fn to_ms(t: SystemTime) -> Option<i64> {
    t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as i64)
}

fn entry_meta(m: &opendal::Metadata) -> EntryMeta {
    EntryMeta {
        size: if m.is_dir() { 0 } else { m.content_length() },
        modified_ms: m.last_modified().and_then(|t| to_ms(t.into())),
        is_dir: m.is_dir(),
        ..Default::default()
    }
}

/// Maps an OpenDAL error to the `io::ErrorKind` the sync executor acts on.
fn io_err(e: opendal::Error) -> io::Error {
    use opendal::ErrorKind as K;
    let kind = match e.kind() {
        K::NotFound => io::ErrorKind::NotFound,
        K::PermissionDenied => io::ErrorKind::PermissionDenied,
        K::AlreadyExists => io::ErrorKind::AlreadyExists,
        K::IsADirectory => io::ErrorKind::IsADirectory,
        K::NotADirectory => io::ErrorKind::NotADirectory,
        K::Unsupported => io::ErrorKind::Unsupported,
        K::ConfigInvalid => io::ErrorKind::InvalidInput,
        K::RateLimited => io::ErrorKind::TimedOut,
        _ if e.is_temporary() => io::ErrorKind::ConnectionReset,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, e.to_string())
}

/// Object-store multipart uploads need parts of at least 5 MiB.
const UPLOAD_CHUNK: usize = 8 << 20;

impl Vfs for OpendalFs {
    fn read_dir(&self, dir: &str) -> Result<Vec<VfsEntry>> {
        let path = dir_path(dir);
        let entries = rt().block_on(self.op.list(&path)).map_err(|e| remote_err(&path, e))?;
        Ok(entries
            .into_iter()
            // Some services include the listed directory itself.
            .filter(|e| e.path() != path && e.path().trim_start_matches('/') != path)
            .map(|e| {
                let name = e.name().trim_end_matches('/').to_owned();
                VfsEntry {
                    hidden: name.starts_with('.'),
                    meta: entry_meta(e.metadata()),
                    name,
                }
            })
            .collect())
    }

    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        let path = path.trim_start_matches('/');
        let reader = self
            .blocking
            .reader(path)
            .and_then(|r| r.into_std_read(..))
            .map_err(|e| remote_err(path, e))?;
        Ok(Box::new(NetRead(reader)))
    }

    fn join(&self, dir: &str, name: &str) -> String {
        let d = dir.trim_matches('/');
        if d.is_empty() {
            name.to_owned()
        } else {
            format!("{d}/{name}")
        }
    }
}

impl OpendalFs {
    /// Deletes a folder tree on a service with real folders: files first,
    /// then folders bottom-up. Some servers report "folder not empty" as
    /// success (OpenDAL's FTP treats every 550 reply as "already gone"), so
    /// the result is checked at the end.
    fn remove_tree(&self, root: &str) -> io::Result<()> {
        let mut pending = vec![root.to_owned()];
        let mut folders = Vec::new();
        while let Some(dir) = pending.pop() {
            let listed = match self.blocking.list(&dir_path(&dir)) {
                Ok(l) => l,
                Err(e) if e.kind() == opendal::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_err(e)),
            };
            for entry in listed {
                let path = entry.path().trim_matches('/').to_owned();
                if path == dir || path.is_empty() {
                    continue; // some services list the folder itself
                }
                if entry.metadata().is_dir() {
                    pending.push(path);
                } else {
                    self.blocking.delete(&path).map_err(io_err)?;
                }
            }
            folders.push(dir);
        }
        for dir in folders.into_iter().rev() {
            self.blocking.delete(&dir_path(&dir)).map_err(io_err)?;
        }
        match self.stat(root)? {
            None => Ok(()),
            Some(_) => Err(io::Error::other(format!("{root} could not be deleted completely"))),
        }
    }
}

impl VfsWrite for OpendalFs {
    fn caps(&self) -> WriteCaps {
        let cap = self.op.info().capability();
        WriteCaps {
            rename: cap.rename,
            set_modified: false,
            // Appending exists for a few services, but never onto a temp
            // name that could then be renamed into place.
            resume_write: false,
            range_read: cap.read,
            folders: matches!(
                self.kind,
                ProviderKind::Ftp
                    | ProviderKind::Webdav
                    | ProviderKind::Onedrive
                    | ProviderKind::Dropbox
                    | ProviderKind::Gdrive
            ),
        }
    }

    fn stat(&self, path: &str) -> io::Result<Option<EntryMeta>> {
        let p = path.trim_matches('/');
        if p.is_empty() {
            return Ok(Some(EntryMeta {
                size: 0,
                modified_ms: None,
                is_dir: true,
                ..Default::default()
            }));
        }
        match self.blocking.stat(p) {
            Ok(m) => Ok(Some(entry_meta(&m))),
            // Folders are addressed with a trailing slash.
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => match self.blocking.stat(&dir_path(p)) {
                Ok(m) => Ok(Some(entry_meta(&m))),
                Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(io_err(e)),
            },
            Err(e) => Err(io_err(e)),
        }
    }

    fn open_at(&self, path: &str, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        let path = path.trim_start_matches('/');
        let reader = self
            .blocking
            .reader(path)
            .and_then(|r| r.into_std_read(offset..))
            .map_err(io_err)?;
        Ok(Box::new(NetRead(reader)))
    }

    fn write(
        &self,
        path: &str,
        offset: u64,
        src: &mut dyn Read,
        on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
    ) -> io::Result<u64> {
        if offset > 0 {
            return Err(unsupported("resuming an upload"));
        }
        let path = path.trim_start_matches('/');
        let cap = self.op.info().capability();
        let opts = opendal::options::WriteOptions {
            chunk: cap.write_can_multi.then_some(UPLOAD_CHUNK),
            ..Default::default()
        };
        // Nothing is visible at `path` until `close` commits the upload, so a
        // failure part-way leaves the old file (or nothing) in place.
        let mut w = self.blocking.writer_options(path, opts).map_err(io_err)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        loop {
            let n = match src.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            w.write(buf[..n].to_vec()).map_err(io_err)?;
            total += n as u64;
            on_bytes(n as u64)?;
        }
        w.close().map_err(io_err)?;
        Ok(total)
    }

    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        let p = path.trim_matches('/');
        if p.is_empty() || !self.op.info().capability().create_dir {
            return Ok(()); // the root always exists; object stores imply folders
        }
        self.blocking.create_dir(&dir_path(p)).map_err(io_err)
    }

    fn remove_file(&self, path: &str) -> io::Result<()> {
        let p = path.trim_start_matches('/');
        self.blocking.delete(p).map_err(io_err)
    }

    fn remove_dir_all(&self, path: &str) -> io::Result<()> {
        let p = path.trim_matches('/');
        if p.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to delete the root folder",
            ));
        }
        if !self.caps().folders {
            // Object stores: one prefix delete, batched by the service.
            let opts = opendal::options::DeleteOptions {
                recursive: true,
                ..Default::default()
            };
            return self.blocking.delete_options(&dir_path(p), opts).map_err(io_err);
        }
        self.remove_tree(p)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let (from, to) = (from.trim_start_matches('/'), to.trim_start_matches('/'));
        self.blocking.rename(from, to).map_err(io_err)
    }

    fn set_modified(&self, _path: &str, _modified_ms: i64) -> io::Result<()> {
        Err(unsupported("setting modification times"))
    }
}
