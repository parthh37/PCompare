//! SFTP via russh — pure Rust, so it builds on Windows without OpenSSH
//! (OpenDAL's SFTP service shells out to `ssh` and is Unix-only).

use crate::profile::ResolvedProfile;
use crate::rt;
use pcompare_core::folder::EntryMeta;
use pcompare_core::vfs::{Vfs, VfsEntry};
use pcompare_core::{CoreError, Result};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg};
use russh::keys::PublicKeyOrCertificate;
use russh_sftp::client::SftpSession;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Host key policy: trust on first use, then pin the SHA-256 fingerprint.
struct Client {
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl russh::client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> std::result::Result<bool, Self::Error> {
        let fp = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256),
            PublicKeyOrCertificate::Certificate(c) => c.public_key().fingerprint(HashAlg::Sha256),
        }
        .to_string();
        let ok = self.expected.as_deref().is_none_or(|e| e == fp);
        *self.seen.lock().unwrap() = Some(fp);
        Ok(ok)
    }
}

pub struct SftpFs {
    sftp: SftpSession,
    root: String,
    // Keeps the SSH connection alive for as long as the file system is.
    _ssh: russh::client::Handle<Client>,
}

pub struct SftpConnection {
    pub fs: SftpFs,
    /// Fingerprint presented by the server, to pin on first connect.
    pub host_key: String,
}

fn err(p: &ResolvedProfile, e: impl std::fmt::Display) -> CoreError {
    CoreError::remote(format!("sftp://{}", p.get("host").unwrap_or("?")), e)
}

fn expand_home(path: &str) -> String {
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => {
            let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
            format!("{home}/{rest}")
        }
        None => path.to_owned(),
    }
}

impl SftpFs {
    pub fn connect(p: &ResolvedProfile) -> Result<SftpConnection> {
        rt().block_on(Self::connect_async(p))
    }

    async fn connect_async(p: &ResolvedProfile) -> Result<SftpConnection> {
        let host = p.get("host").ok_or_else(|| err(p, "host is required"))?;
        let port: u16 = p.get("port").unwrap_or("22").parse().map_err(|_| err(p, "invalid port"))?;
        let user = p.get("user").ok_or_else(|| err(p, "user is required"))?;
        let expected = p.get("host_key").map(str::to_owned);
        let seen = Arc::new(Mutex::new(None));

        let config = russh::client::Config {
            keepalive_interval: Some(Duration::from_secs(30)),
            ..Default::default()
        };
        let handler = Client {
            expected: expected.clone(),
            seen: seen.clone(),
        };
        let connect = russh::client::connect(Arc::new(config), (host, port), handler);
        let mut ssh = match tokio::time::timeout(Duration::from_secs(20), connect).await {
            Err(_) => return Err(err(p, "connection timed out")),
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                let got = seen.lock().unwrap().clone();
                return Err(match (expected, got) {
                    (Some(want), Some(got)) if want != got => err(
                        p,
                        format!(
                            "HOST KEY CHANGED — expected {want}, server sent {got}. \
                             This may be an attack; clear 'Trusted host key' only if you know the server was reinstalled."
                        ),
                    ),
                    _ => err(p, e),
                });
            }
        };

        let auth = match p.get("auth").unwrap_or("password") {
            "key" => {
                let path = expand_home(p.get("private_key_path").ok_or_else(|| err(p, "private key file is required"))?);
                let key = russh::keys::load_secret_key(&path, p.get("passphrase"))
                    .map_err(|e| err(p, format!("cannot load key {path}: {e}")))?;
                let hash = ssh.best_supported_rsa_hash().await.map_err(|e| err(p, e))?.flatten();
                ssh.authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                    .await
            }
            _ => ssh.authenticate_password(user, p.get("password").unwrap_or_default()).await,
        }
        .map_err(|e| err(p, e))?;
        if !auth.success() {
            return Err(err(p, format!("authentication failed for user `{user}`")));
        }

        let channel = ssh.channel_open_session().await.map_err(|e| err(p, e))?;
        channel.request_subsystem(true, "sftp").await.map_err(|e| err(p, e))?;
        let sftp = SftpSession::new(channel.into_stream()).await.map_err(|e| err(p, e))?;

        let root = match p.get("root") {
            None | Some("~") => sftp.canonicalize(".").await.map_err(|e| err(p, e))?,
            Some(r) => match r.strip_prefix("~/") {
                Some(rest) => format!("{}/{rest}", sftp.canonicalize(".").await.map_err(|e| err(p, e))?),
                None => r.to_owned(),
            },
        };
        let host_key = seen.lock().unwrap().clone().unwrap_or_default();
        Ok(SftpConnection {
            fs: SftpFs { sftp, root, _ssh: ssh },
            host_key,
        })
    }

    fn full(&self, rel: &str) -> String {
        let rel = rel.trim_matches('/');
        let root = self.root.trim_end_matches('/');
        match (root.is_empty(), rel.is_empty()) {
            (_, true) if root.is_empty() => "/".into(),
            (_, true) => root.to_owned(),
            (true, false) => format!("/{rel}"),
            (false, false) => format!("{root}/{rel}"),
        }
    }
}

impl Vfs for SftpFs {
    fn read_dir(&self, dir: &str) -> Result<Vec<VfsEntry>> {
        let full = self.full(dir);
        rt().block_on(async {
            let listing = self.sftp.read_dir(full.clone()).await.map_err(|e| CoreError::remote(&full, e))?;
            let mut out = Vec::new();
            for entry in listing {
                let name = entry.file_name();
                let mut meta = entry.metadata();
                if meta.is_symlink() {
                    // Follow links so linked files compare by their target.
                    if let Ok(target) = self.sftp.metadata(format!("{}/{name}", full.trim_end_matches('/'))).await {
                        meta = target;
                    }
                }
                out.push(VfsEntry {
                    hidden: name.starts_with('.'),
                    meta: EntryMeta {
                        size: if meta.is_dir() { 0 } else { meta.size.unwrap_or(0) },
                        modified_ms: meta.mtime.map(|s| s as i64 * 1000),
                        is_dir: meta.is_dir(),
                    },
                    name,
                });
            }
            Ok(out)
        })
    }

    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        let full = self.full(path);
        let file = rt().block_on(self.sftp.open(full.clone())).map_err(|e| CoreError::remote(&full, e))?;
        Ok(Box::new(tokio_util::io::SyncIoBridge::new_with_handle(file, rt().handle().clone())))
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
