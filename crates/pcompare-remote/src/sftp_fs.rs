//! SFTP via russh — pure Rust, so it builds on Windows without OpenSSH
//! (OpenDAL's SFTP service shells out to `ssh` and is Unix-only).
//!
//! One SSH connection per profile carries a small pool of SFTP channels, so
//! the left side, the right side and a running sync can transfer at the same
//! time instead of queueing behind one channel's flow-control window. A
//! connection that drops is re-established on the next call (presenting the
//! same pinned host key), so a long sync survives a flaky link.

use crate::profile::ResolvedProfile;
use crate::prompt::{is_password_prompt, AuthPrompt, PromptField, Prompter};
use crate::{net_err, rt, NetRead};
use pcompare_core::folder::EntryMeta;
use pcompare_core::vfs::{Vfs, VfsEntry};
use pcompare_core::vfs_write::{VfsWrite, WriteCaps};
use pcompare_core::{CoreError, Result};
use russh::client::{AuthResult, KeyboardInteractiveAuthResponse};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::agent::AgentIdentity;
use russh::keys::PublicKeyOrCertificate;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg};
use russh::MethodKind;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use std::io::{self, Read, SeekFrom};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::SyncIoBridge;

/// SFTP channels opened per connection.
const CHANNELS: usize = 3;
/// Per-request timeout. Generous: under load, writes queue behind each other.
const REQUEST_TIMEOUT_SECS: u64 = 60;
/// Keyboard-interactive rounds before giving up on a chatty server.
const MAX_PROMPT_ROUNDS: usize = 8;

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

/// A live SSH connection and its SFTP channels.
struct Conn {
    channels: Vec<SftpSession>,
    ssh: russh::client::Handle<Client>,
}

pub struct SftpFs {
    /// For reconnecting; carries the host key pinned on the first connect.
    profile: ResolvedProfile,
    prompter: Option<Arc<dyn Prompter>>,
    root: String,
    conn: tokio::sync::Mutex<Arc<Conn>>,
    next: AtomicUsize,
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
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_default();
            format!("{home}/{rest}")
        }
        None => path.to_owned(),
    }
}

/// Maps an SFTP failure to the `io::ErrorKind` the sync executor acts on.
fn sftp_io(e: SftpError) -> io::Error {
    let kind = match &e {
        SftpError::Status(s) => match s.status_code {
            StatusCode::NoSuchFile => io::ErrorKind::NotFound,
            StatusCode::PermissionDenied => io::ErrorKind::PermissionDenied,
            StatusCode::NoConnection | StatusCode::ConnectionLost => io::ErrorKind::ConnectionReset,
            StatusCode::OpUnsupported => io::ErrorKind::Unsupported,
            StatusCode::Eof => io::ErrorKind::UnexpectedEof,
            _ => io::ErrorKind::Other,
        },
        SftpError::Timeout => io::ErrorKind::TimedOut,
        SftpError::IO(_) | SftpError::UnexpectedBehavior(_) => io::ErrorKind::ConnectionReset,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, e.to_string())
}

fn meta_of(m: &FileAttributes) -> EntryMeta {
    EntryMeta {
        size: if m.is_dir() { 0 } else { m.size.unwrap_or(0) },
        modified_ms: m.mtime.map(|s| s as i64 * 1000),
        is_dir: m.is_dir(),
        ..Default::default()
    }
}

type Agent = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

#[cfg(unix)]
async fn connect_agent(p: &ResolvedProfile) -> Result<Agent> {
    let sock = match p.get("agent_socket") {
        Some(s) => expand_home(s),
        None => std::env::var("SSH_AUTH_SOCK").map_err(|_| {
            err(p, "no SSH agent found: SSH_AUTH_SOCK isn't set. Start ssh-agent, or enter the agent socket in the profile.")
        })?,
    };
    AgentClient::connect_uds(&sock)
        .await
        .map(|a| a.dynamic())
        .map_err(|e| err(p, format!("cannot reach the SSH agent at {sock}: {e}")))
}

/// Windows: the OpenSSH agent service's named pipe, else Pageant (PuTTY).
#[cfg(windows)]
async fn connect_agent(p: &ResolvedProfile) -> Result<Agent> {
    const OPENSSH_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
    let pipe = p.get("agent_socket").unwrap_or(OPENSSH_PIPE);
    match AgentClient::connect_named_pipe(pipe).await {
        Ok(a) => Ok(a.dynamic()),
        Err(pipe_err) if p.get("agent_socket").is_none() => AgentClient::connect_pageant()
            .await
            .map(|a| a.dynamic())
            .map_err(|e| {
                err(
                    p,
                    format!(
                        "no SSH agent found: the OpenSSH Authentication Agent service isn't running ({pipe_err}) and neither is Pageant ({e})"
                    ),
                )
            }),
        Err(e) => Err(err(p, format!("cannot reach the SSH agent at {pipe}: {e}"))),
    }
}

/// Tries every key the agent holds, like `ssh` does.
async fn agent_auth(ssh: &mut russh::client::Handle<Client>, p: &ResolvedProfile, user: &str) -> Result<bool> {
    let mut agent = connect_agent(p).await?;
    let ids = agent
        .request_identities()
        .await
        .map_err(|e| err(p, format!("SSH agent: {e}")))?;
    if ids.is_empty() {
        return Err(err(p, "the SSH agent holds no keys (add one with ssh-add)"));
    }
    let hash = ssh.best_supported_rsa_hash().await.map_err(|e| err(p, e))?.flatten();
    for id in ids {
        let res = match id {
            AgentIdentity::PublicKey { key, .. } => ssh.authenticate_publickey_with(user, key, hash, &mut agent).await,
            AgentIdentity::Certificate { certificate, .. } => {
                ssh.authenticate_certificate_with(user, certificate, hash, &mut agent)
                    .await
            }
        }
        .map_err(|e| err(p, format!("SSH agent: {e}")))?;
        if res.success() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Keyboard-interactive: password prompts are answered from the saved
/// password (once — a repeated prompt means it was wrong), anything else is
/// put to the user through the prompter.
async fn keyboard_interactive(
    ssh: &mut russh::client::Handle<Client>,
    p: &ResolvedProfile,
    user: &str,
    prompter: Option<&Arc<dyn Prompter>>,
) -> Result<bool> {
    let mut saved = p.get("password");
    let mut resp = ssh
        .authenticate_keyboard_interactive_start(user, None::<String>)
        .await
        .map_err(|e| err(p, e))?;
    for _ in 0..MAX_PROMPT_ROUNDS {
        let (name, instructions, prompts) = match resp {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => (name, instructions, prompts),
        };
        let fields: Vec<PromptField> = prompts
            .into_iter()
            .map(|q| PromptField {
                prompt: q.prompt,
                echo: q.echo,
            })
            .collect();
        let answers = if fields.is_empty() {
            Vec::new()
        } else if let Some(pw) = saved.filter(|_| fields.iter().all(is_password_prompt)) {
            saved = None;
            vec![pw.to_owned(); fields.len()]
        } else {
            let Some(prompter) = prompter.cloned() else {
                return Err(err(
                    p,
                    format!(
                        "the server asks \"{}\", which needs an answer from you",
                        fields[0].prompt.trim()
                    ),
                ));
            };
            let n = fields.len();
            let question = AuthPrompt {
                profile: p.profile.name.clone(),
                target: format!("{user}@{}", p.get("host").unwrap_or("?")),
                name,
                instructions,
                fields,
            };
            // The prompter blocks until the user answers; keep it off the runtime's workers.
            match tokio::task::spawn_blocking(move || prompter.ask(&question)).await {
                Ok(Some(a)) if a.len() == n => a,
                _ => return Err(err(p, "sign-in cancelled")),
            }
        };
        resp = ssh
            .authenticate_keyboard_interactive_respond(answers)
            .await
            .map_err(|e| err(p, e))?;
    }
    Ok(false)
}

async fn authenticate(
    ssh: &mut russh::client::Handle<Client>,
    p: &ResolvedProfile,
    user: &str,
    prompter: Option<&Arc<dyn Prompter>>,
) -> Result<()> {
    let ok = match p.get("auth").unwrap_or("password") {
        "key" => {
            let path = expand_home(
                p.get("private_key_path")
                    .ok_or_else(|| err(p, "private key file is required"))?,
            );
            let key = russh::keys::load_secret_key(&path, p.get("passphrase"))
                .map_err(|e| err(p, format!("cannot load key {path}: {e}")))?;
            let hash = ssh.best_supported_rsa_hash().await.map_err(|e| err(p, e))?.flatten();
            ssh.authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(|e| err(p, e))?
                .success()
        }
        "agent" => agent_auth(ssh, p, user).await?,
        "keyboard-interactive" => keyboard_interactive(ssh, p, user, prompter).await?,
        _ => match ssh
            .authenticate_password(user, p.get("password").unwrap_or_default())
            .await
            .map_err(|e| err(p, e))?
        {
            AuthResult::Success => true,
            // Many servers (PAM) only take passwords through keyboard-interactive.
            AuthResult::Failure { remaining_methods, .. }
                if remaining_methods.contains(&MethodKind::KeyboardInteractive) =>
            {
                keyboard_interactive(ssh, p, user, prompter).await?
            }
            _ => false,
        },
    };
    if ok {
        Ok(())
    } else {
        Err(err(p, format!("authentication failed for user `{user}`")))
    }
}

/// Connects, authenticates and opens the channel pool. Returns the server's
/// host key fingerprint.
async fn open(p: &ResolvedProfile, prompter: Option<&Arc<dyn Prompter>>) -> Result<(Conn, String)> {
    let host = p.get("host").ok_or_else(|| err(p, "host is required"))?;
    let port: u16 = p
        .get("port")
        .unwrap_or("22")
        .parse()
        .map_err(|_| err(p, "invalid port"))?;
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

    authenticate(&mut ssh, p, user, prompter).await?;

    let mut channels = Vec::with_capacity(CHANNELS);
    for _ in 0..CHANNELS {
        let channel = ssh.channel_open_session().await.map_err(|e| err(p, e))?;
        channel.request_subsystem(true, "sftp").await.map_err(|e| err(p, e))?;
        let sftp = SftpSession::new(channel.into_stream()).await.map_err(|e| err(p, e))?;
        sftp.set_timeout(REQUEST_TIMEOUT_SECS);
        channels.push(sftp);
    }
    let host_key = seen.lock().unwrap().clone().unwrap_or_default();
    Ok((Conn { channels, ssh }, host_key))
}

async fn resolve_root(p: &ResolvedProfile, sftp: &SftpSession) -> Result<String> {
    Ok(match p.get("root") {
        None | Some("~") => sftp.canonicalize(".").await.map_err(|e| err(p, e))?,
        Some(r) => match r.strip_prefix("~/") {
            Some(rest) => format!("{}/{rest}", sftp.canonicalize(".").await.map_err(|e| err(p, e))?),
            None => r.to_owned(),
        },
    })
}

/// A reader that keeps its connection alive while it is in use.
struct Held<R> {
    inner: R,
    _conn: Arc<Conn>,
}

impl<R: Read> Read for Held<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl SftpFs {
    pub fn connect(p: &ResolvedProfile, prompter: Option<Arc<dyn Prompter>>) -> Result<SftpConnection> {
        rt().block_on(async {
            let (conn, host_key) = open(p, prompter.as_ref()).await?;
            let root = resolve_root(p, &conn.channels[0]).await?;
            // Reconnects must meet the same server.
            let mut profile = p.clone();
            profile.profile.settings.insert("host_key".into(), host_key.clone());
            Ok(SftpConnection {
                fs: SftpFs {
                    profile,
                    prompter,
                    root,
                    conn: tokio::sync::Mutex::new(Arc::new(conn)),
                    next: AtomicUsize::new(0),
                },
                host_key,
            })
        })
    }

    /// The live connection (re-established if it dropped) and the channel
    /// to use for the next request, round-robin.
    async fn session(&self) -> io::Result<(Arc<Conn>, usize)> {
        let mut conn = self.conn.lock().await;
        if conn.ssh.is_closed() {
            let (fresh, _) = open(&self.profile, self.prompter.as_ref())
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::ConnectionReset, format!("reconnecting: {e}")))?;
            *conn = Arc::new(fresh);
        }
        let c = conn.clone();
        let i = self.next.fetch_add(1, Ordering::Relaxed) % c.channels.len();
        Ok((c, i))
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

    async fn remove_tree(sftp: &SftpSession, full: &str) -> io::Result<()> {
        let meta = match sftp.symlink_metadata(full.to_owned()).await {
            Ok(m) => m,
            Err(e) => {
                let e = sftp_io(e);
                return if e.kind() == io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(e)
                };
            }
        };
        if !meta.is_dir() {
            return sftp.remove_file(full.to_owned()).await.map_err(sftp_io);
        }
        // Depth-first: files go as they are found, folders once emptied.
        let mut pending = vec![full.to_owned()];
        let mut folders = Vec::new();
        while let Some(dir) = pending.pop() {
            for entry in sftp.read_dir(dir.clone()).await.map_err(sftp_io)? {
                let path = format!("{}/{}", dir.trim_end_matches('/'), entry.file_name());
                // Listing attributes describe links themselves, so a link to
                // a folder is unlinked, never followed.
                if entry.metadata().is_dir() {
                    pending.push(path);
                } else {
                    sftp.remove_file(path).await.map_err(sftp_io)?;
                }
            }
            folders.push(dir);
        }
        for dir in folders.into_iter().rev() {
            sftp.remove_dir(dir).await.map_err(sftp_io)?;
        }
        Ok(())
    }
}

impl Vfs for SftpFs {
    fn read_dir(&self, dir: &str) -> Result<Vec<VfsEntry>> {
        let full = self.full(dir);
        rt().block_on(async {
            let (c, i) = self.session().await?;
            let sftp = &c.channels[i];
            let listing = sftp.read_dir(full.clone()).await.map_err(sftp_io)?;
            let mut out = Vec::new();
            for entry in listing {
                let name = entry.file_name();
                let mut meta = entry.metadata();
                if meta.is_symlink() {
                    // Follow links so linked files compare by their target.
                    if let Ok(target) = sftp.metadata(format!("{}/{name}", full.trim_end_matches('/'))).await {
                        meta = target;
                    }
                }
                out.push(VfsEntry {
                    hidden: name.starts_with('.'),
                    meta: meta_of(&meta),
                    name,
                });
            }
            Ok(out)
        })
        .map_err(|e: io::Error| CoreError::remote(&full, e))
    }

    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        self.open_at(path, 0).map_err(|e| CoreError::remote(self.full(path), e))
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

impl VfsWrite for SftpFs {
    fn caps(&self) -> WriteCaps {
        WriteCaps {
            rename: true,
            set_modified: true,
            resume_write: true,
            range_read: true,
            folders: true,
            serial_writes: false,
        }
    }

    fn stat(&self, path: &str) -> io::Result<Option<EntryMeta>> {
        let full = self.full(path);
        rt().block_on(async {
            let (c, i) = self.session().await?;
            match c.channels[i].metadata(full).await.map_err(sftp_io) {
                Ok(m) => Ok(Some(meta_of(&m))),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        })
    }

    fn open_at(&self, path: &str, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        let full = self.full(path);
        let (conn, file) = rt().block_on(async {
            let (c, i) = self.session().await?;
            let mut file = c.channels[i].open(full).await.map_err(sftp_io)?;
            if offset > 0 {
                file.seek(SeekFrom::Start(offset)).await?;
            }
            Ok::<_, io::Error>((c, file))
        })?;
        Ok(Box::new(Held {
            inner: NetRead(SyncIoBridge::new_with_handle(file, rt().handle().clone())),
            _conn: conn,
        }))
    }

    fn write(
        &self,
        path: &str,
        offset: u64,
        src: &mut dyn Read,
        on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
    ) -> io::Result<u64> {
        let full = self.full(path);
        let (_conn, mut file) = rt().block_on(async {
            let (c, i) = self.session().await?;
            let sftp = &c.channels[i];
            let file = if offset == 0 {
                sftp.create(full).await.map_err(sftp_io)?
            } else {
                let mut f = sftp.open_with_flags(full, OpenFlags::WRITE).await.map_err(sftp_io)?;
                let size = f.metadata().await.map_err(sftp_io)?.size.unwrap_or(0);
                if size != offset {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("the partial upload holds {size} bytes, expected {offset}"),
                    ));
                }
                f.seek(SeekFrom::Start(offset)).await?;
                f
            };
            Ok::<_, io::Error>((c, file))
        })?;
        let mut buf = vec![0u8; 256 << 10];
        let mut total = 0u64;
        loop {
            let n = match src.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            rt().block_on(file.write_all(&buf[..n])).map_err(net_err)?;
            total += n as u64;
            on_bytes(n as u64)?;
        }
        // Waits for every pending write to be acknowledged, then closes.
        rt().block_on(file.shutdown()).map_err(net_err)?;
        Ok(total)
    }

    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        let rel = path.trim_matches('/');
        if rel.is_empty() {
            return Ok(());
        }
        rt().block_on(async {
            let (c, i) = self.session().await?;
            let sftp = &c.channels[i];
            let mut cur = String::new();
            for part in rel.split('/').filter(|p| !p.is_empty()) {
                cur = self.join(&cur, part);
                let full = self.full(&cur);
                match sftp.metadata(full.clone()).await.map_err(sftp_io) {
                    Ok(m) if m.is_dir() => continue,
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::NotADirectory,
                            format!("{full} is a file, not a folder"),
                        ))
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        if let Err(e) = sftp.create_dir(full.clone()).await {
                            // Someone else may have created it meanwhile.
                            if !sftp.metadata(full).await.is_ok_and(|m| m.is_dir()) {
                                return Err(sftp_io(e));
                            }
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })
    }

    fn remove_file(&self, path: &str) -> io::Result<()> {
        let full = self.full(path);
        rt().block_on(async {
            let (c, i) = self.session().await?;
            match c.channels[i].remove_file(full).await.map_err(sftp_io) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        })
    }

    fn remove_dir_all(&self, path: &str) -> io::Result<()> {
        if path.trim_matches('/').is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to delete the root folder",
            ));
        }
        let full = self.full(path);
        rt().block_on(async {
            let (c, i) = self.session().await?;
            Self::remove_tree(&c.channels[i], &full).await
        })
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let (from, to) = (self.full(from), self.full(to));
        rt().block_on(async {
            let (c, i) = self.session().await?;
            c.channels[i].rename(from, to).await.map_err(sftp_io)
        })
    }

    fn set_modified(&self, path: &str, modified_ms: i64) -> io::Result<()> {
        let full = self.full(path);
        let secs = (modified_ms.max(0) / 1000) as u32;
        let attrs = FileAttributes {
            atime: Some(secs),
            mtime: Some(secs),
            ..FileAttributes::empty()
        };
        rt().block_on(async {
            let (c, i) = self.session().await?;
            c.channels[i].set_metadata(full, attrs).await.map_err(sftp_io)
        })
    }
}
