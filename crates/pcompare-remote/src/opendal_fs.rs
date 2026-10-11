//! Object stores and cloud drives through Apache OpenDAL.

use crate::profile::ResolvedProfile;
use crate::provider::{spec, FieldKind, ProviderKind};
use crate::rt;
use pcompare_core::folder::EntryMeta;
use pcompare_core::vfs::{Vfs, VfsEntry};
use pcompare_core::{CoreError, Result};
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct OpendalFs {
    op: opendal::Operator,
    blocking: opendal::blocking::Operator,
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
            put("endpoint", &format!("{}://{host}:{port}", if secure { "ftps" } else { "ftp" }));
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
        Ok(Self { op, blocking })
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

impl Vfs for OpendalFs {
    fn read_dir(&self, dir: &str) -> Result<Vec<VfsEntry>> {
        let path = dir_path(dir);
        let entries = rt()
            .block_on(self.op.list(&path))
            .map_err(|e| remote_err(&path, e))?;
        Ok(entries
            .into_iter()
            // Some services include the listed directory itself.
            .filter(|e| e.path() != path && e.path().trim_start_matches('/') != path)
            .map(|e| {
                let m = e.metadata();
                let name = e.name().trim_end_matches('/').to_owned();
                VfsEntry {
                    hidden: name.starts_with('.'),
                    meta: EntryMeta {
                        size: if m.is_dir() { 0 } else { m.content_length() },
                        modified_ms: m.last_modified().and_then(|t| to_ms(t.into())),
                        is_dir: m.is_dir(),
                    },
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
        Ok(Box::new(reader))
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
