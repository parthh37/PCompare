//! Live round-trips against real servers. Skipped unless
//! `PCOMPARE_LIVE_PROFILES` points to a JSON file like:
//!
//! ```json
//! [{ "profile": { "name": "local sftp", "kind": "sftp",
//!                 "settings": { "host": "127.0.0.1", "port": "2222", "user": "me",
//!                               "auth": "key", "private_key_path": "/tmp/k", "root": "/tmp/data" },
//!                 "secrets": {} } }]
//! ```
//!
//! Each server's root must contain this fixture:
//! `left/a.txt` = `right/a.txt` = "hello\n", `left/sub/b.txt` = "one\n",
//! `right/sub/b.txt` = "two!\n", and `left/orphan.txt`.
//!
//! Besides reading, every profile is exercised for writing:
//! file and folder operations, ranged reads and offset writes, parallel reads
//! over the shared connection, and Folder Sync in both directions — including
//! transfers that drop part-way and resume. Everything written goes into
//! `pcompare-live-*` folders in the root, which are removed again. Add
//! `"readOnly": true` to a case to skip the write tests. SFTP profiles may use
//! `"auth": "agent"` (with `SSH_AUTH_SOCK` or an `agent_socket` setting).

use pcompare_core::folder::{compare_folders_with, ContentMethod, FolderCompareOptions};
use pcompare_core::vfs::{LocalFs, Vfs, VfsEntry};
use pcompare_core::vfs_write::{VfsWrite, WriteCaps};
use pcompare_remote::profile::{MemorySecrets, ProfileInput};
use pcompare_remote::provider::ProviderKind;
use pcompare_remote::RemoteManager;
use pcompare_sync::exec::{execute_on, ExecOptions, ExecReport};
use pcompare_sync::fsops::PermanentDeleter;
use pcompare_sync::journal::NullJournal;
use pcompare_sync::remote::Root;
use pcompare_sync::sync::{plan_sync_with, PlanOptions, SyncMode, SyncPlan};
use serde::Deserialize;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    profile: ProfileInput,
    #[serde(default)]
    read_only: bool,
}

#[test]
fn live_profiles_roundtrip() {
    let Ok(path) = std::env::var("PCOMPARE_LIVE_PROFILES") else {
        eprintln!("PCOMPARE_LIVE_PROFILES not set; skipping");
        return;
    };
    let cases: Vec<Case> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let m = RemoteManager::new(dir.path(), dir.path().join("cache"), Box::<MemorySecrets>::default());

    for (n, case) in cases.into_iter().enumerate() {
        let name = case.profile.name.clone();
        let msg = m
            .test(&case.profile)
            .unwrap_or_else(|e| panic!("{name}: test failed: {e}"));
        eprintln!("{name}: {msg}");
        let p = m.profiles.save(case.profile).unwrap();
        let base = format!("remote://{}", p.id);

        let names: Vec<String> = m
            .list(&format!("{base}/left"))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            names,
            ["sub", "a.txt", "orphan.txt"],
            "{name}: folders first, then by name"
        );

        let opts = FolderCompareOptions {
            content: ContentMethod::Binary,
            ..Default::default()
        };
        let (lfs, l) = m.target(&format!("{base}/left")).unwrap();
        let (rfs, r) = m.target(&format!("{base}/right")).unwrap();
        let res = compare_folders_with(&*lfs, &l, &*rfs, &r, &opts).unwrap();
        assert_eq!(
            (res.stats.same, res.stats.different, res.stats.left_only),
            (1, 1, 1),
            "{name}"
        );

        let copy = m.local_copy(&format!("{base}/right/sub/b.txt"), true).unwrap();
        assert_eq!(std::fs::read_to_string(copy).unwrap(), "two!\n", "{name}");

        parallel_reads(&m, &base, &name);
        if !case.read_only {
            let tag = format!("{}-{n}", std::process::id());
            file_operations(&m, &base, &tag, &name);
            sync_both_ways(&m, &base, &tag, &name);
        }

        // SFTP pins the host key on first real use.
        if p.kind == ProviderKind::Sftp {
            assert!(
                m.profiles.get(&p.id).unwrap().settings.contains_key("host_key"),
                "{name}: host key pinned"
            );
            // A different key afterwards must be refused.
            m.profiles
                .set_setting(&p.id, "host_key", "SHA256:not-the-real-key")
                .unwrap();
            m.invalidate(&p.id);
            let err = m
                .list(&format!("{base}/left"))
                .expect_err("tampered host key must fail")
                .to_string();
            assert!(err.contains("HOST KEY CHANGED"), "{name}: {err}");
        }
        eprintln!("{name}: ok");
    }
}

fn read_all(mut r: impl Read) -> String {
    let mut s = String::new();
    r.read_to_string(&mut s).unwrap();
    s
}

/// Left, right and a sync share one connection per profile: many readers
/// at once must each get their own, intact data.
fn parallel_reads(m: &RemoteManager, base: &str, name: &str) {
    let (fs, root) = m.target(base).unwrap();
    let file = |rel: &str| rel.split('/').fold(root.clone(), |acc, part| fs.join(&acc, part));
    let (a, b) = (file("left/a.txt"), file("right/sub/b.txt"));
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                for _ in 0..4 {
                    assert_eq!(read_all(fs.open(&a).unwrap()), "hello\n", "{name}");
                    assert_eq!(read_all(fs.open(&b).unwrap()), "two!\n", "{name}");
                }
            });
        }
    });
}

fn file_operations(m: &RemoteManager, base: &str, tag: &str, name: &str) {
    let scratch = format!("{base}/pcompare-live-{tag}");
    m.create_dir(&format!("{scratch}/w/deep")).unwrap();
    let (fs, root) = m.target_write(&scratch).unwrap().unwrap();
    let caps = fs.caps();
    let w = fs.join(&root, "w");
    let a = fs.join(&w, "a.txt");

    let n = fs.write(&a, 0, &mut &b"hello world"[..], &mut |_| Ok(())).unwrap();
    assert_eq!(n, 11, "{name}");
    assert_eq!(
        fs.stat(&a).unwrap().map(|m| (m.size, m.is_dir)),
        Some((11, false)),
        "{name}"
    );
    assert!(
        fs.stat(&fs.join(&w, "deep")).unwrap().is_some_and(|m| m.is_dir),
        "{name}: folder created"
    );
    assert!(fs.stat(&fs.join(&w, "nothing")).unwrap().is_none(), "{name}");
    assert_eq!(read_all(fs.open_at(&a, 6).unwrap()), "world", "{name}: ranged read");

    if caps.resume_write {
        let halves = fs.join(&w, "halves.txt");
        fs.write(&halves, 0, &mut &b"abc"[..], &mut |_| Ok(())).unwrap();
        fs.write(&halves, 3, &mut &b"def"[..], &mut |_| Ok(())).unwrap();
        assert_eq!(read_all(fs.open(&halves).unwrap()), "abcdef", "{name}: offset write");
    }
    if caps.set_modified {
        fs.set_modified(&a, 1_600_000_000_000).unwrap();
        assert_eq!(
            fs.stat(&a).unwrap().unwrap().modified_ms,
            Some(1_600_000_000_000),
            "{name}"
        );
    }
    let current = if caps.rename {
        m.rename(&format!("{scratch}/w/a.txt"), "b.txt").unwrap();
        let names: Vec<String> = m
            .list(&format!("{scratch}/w"))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(
            names.contains(&"b.txt".to_owned()) && !names.contains(&"a.txt".to_owned()),
            "{name}: {names:?}"
        );
        "b.txt"
    } else {
        assert!(
            m.rename(&format!("{scratch}/w/a.txt"), "b.txt").is_err(),
            "{name}: rename refused"
        );
        "a.txt"
    };
    m.delete(&format!("{scratch}/w/{current}")).unwrap();
    assert!(
        fs.stat(&fs.join(&w, current)).unwrap().is_none(),
        "{name}: file deleted"
    );
    m.delete(&scratch).unwrap();
    let left: Vec<String> = m.list(base).unwrap().into_iter().map(|e| e.name).collect();
    assert!(
        !left.iter().any(|n| n.starts_with("pcompare-live-")),
        "{name}: scratch removed: {left:?}"
    );
}

/// A remote whose first transfer (each way) drops after `after` bytes, as a
/// flaky connection would.
struct Flaky {
    inner: Arc<dyn VfsWrite>,
    read_drops: Mutex<u32>,
    write_drops: Mutex<u32>,
    after: u64,
    read_offsets: Mutex<Vec<u64>>,
    write_offsets: Mutex<Vec<u64>>,
}

impl Flaky {
    fn new(inner: Arc<dyn VfsWrite>, after: u64) -> Self {
        Flaky {
            inner,
            read_drops: Mutex::new(1),
            write_drops: Mutex::new(1),
            after,
            read_offsets: Mutex::default(),
            write_offsets: Mutex::default(),
        }
    }
}

fn take(n: &Mutex<u32>) -> bool {
    let mut left = n.lock().unwrap();
    let drop = *left > 0;
    *left = left.saturating_sub(1);
    drop
}

fn reset() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionReset, "simulated dropped connection")
}

struct Cut<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Cut<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Err(reset());
        }
        let n = buf.len().min(self.left as usize);
        let n = self.inner.read(&mut buf[..n])?;
        self.left -= n as u64;
        Ok(n)
    }
}

impl Vfs for Flaky {
    fn read_dir(&self, dir: &str) -> pcompare_core::Result<Vec<VfsEntry>> {
        self.inner.read_dir(dir)
    }
    fn open(&self, path: &str) -> pcompare_core::Result<Box<dyn Read + Send>> {
        self.inner.open(path)
    }
    fn join(&self, dir: &str, name: &str) -> String {
        self.inner.join(dir, name)
    }
}

impl VfsWrite for Flaky {
    fn caps(&self) -> WriteCaps {
        self.inner.caps()
    }
    fn stat(&self, path: &str) -> io::Result<Option<pcompare_core::folder::EntryMeta>> {
        self.inner.stat(path)
    }
    fn open_at(&self, path: &str, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        self.read_offsets.lock().unwrap().push(offset);
        let r = self.inner.open_at(path, offset)?;
        Ok(if take(&self.read_drops) {
            Box::new(Cut {
                inner: r,
                left: self.after,
            })
        } else {
            r
        })
    }
    fn write(
        &self,
        path: &str,
        offset: u64,
        src: &mut dyn Read,
        on_bytes: &mut dyn FnMut(u64) -> io::Result<()>,
    ) -> io::Result<u64> {
        self.write_offsets.lock().unwrap().push(offset);
        if take(&self.write_drops) {
            if self.inner.caps().rename {
                // The bytes sent so far land in the (temp) file being written.
                self.inner.write(path, offset, &mut src.take(self.after), on_bytes)?;
            } else {
                // An object-store upload that dies is never committed.
                io::copy(&mut src.take(self.after), &mut io::sink())?;
            }
            return Err(reset());
        }
        self.inner.write(path, offset, src, on_bytes)
    }
    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        self.inner.create_dir_all(path)
    }
    fn remove_file(&self, path: &str) -> io::Result<()> {
        self.inner.remove_file(path)
    }
    fn remove_dir_all(&self, path: &str) -> io::Result<()> {
        self.inner.remove_dir_all(path)
    }
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to)
    }
    fn set_modified(&self, path: &str, ms: i64) -> io::Result<()> {
        self.inner.set_modified(path, ms)
    }
}

fn write_local(root: &Path, rel: &str, body: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// Pseudo-random bytes, so a misplaced resume shows up as corruption.
fn noise(len: usize) -> Vec<u8> {
    let mut x: u32 = 0x2545_f491;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn exec_opts() -> ExecOptions {
    ExecOptions {
        retry_delay_ms: 10,
        retry_max_delay_ms: 50,
        verify: true,
        ..Default::default()
    }
}

fn plan(lfs: &dyn Vfs, l: &str, rfs: &dyn Vfs, r: &str, opts: &FolderCompareOptions) -> SyncPlan {
    plan_sync_with(
        lfs,
        l,
        rfs,
        r,
        SyncMode::MirrorLeftToRight,
        opts,
        &PlanOptions::default(),
        None,
    )
    .unwrap()
}

fn run(left: Root, right: Root, plan: &SyncPlan, name: &str) -> ExecReport {
    let r = execute_on(
        left,
        right,
        &plan.ops,
        &exec_opts(),
        &PermanentDeleter,
        &mut NullJournal,
        &AtomicBool::new(false),
        &mut |_| {},
    );
    assert_eq!(r.failed, 0, "{name}: {:?}", r.results);
    r
}

fn sync_both_ways(m: &RemoteManager, base: &str, tag: &str, name: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let (src, back, resumed) = (
        tmp.path().join("src"),
        tmp.path().join("back"),
        tmp.path().join("resumed"),
    );
    let big = noise(700_000);
    write_local(&src, "a.txt", b"one");
    write_local(&src, "sub/b.txt", b"two");
    write_local(&src, "sub/deeper/big.bin", &big);
    write_local(&src, "gone.txt", b"bye");
    for d in [&back, &resumed] {
        std::fs::create_dir_all(d).unwrap();
    }
    let (src_s, back_s, resumed_s) = (src.to_string_lossy(), back.to_string_lossy(), resumed.to_string_lossy());

    let uri = format!("{base}/pcompare-live-sync-{tag}");
    m.create_dir(&uri).unwrap();
    let (fs, rel) = m.target_write(&uri).unwrap().unwrap();
    let remote = Root::Remote {
        fs: &*fs,
        path: &rel,
        label: &uri,
    };
    let quick = FolderCompareOptions::default();
    let by_content = FolderCompareOptions {
        content: ContentMethod::Binary,
        ..Default::default()
    };

    // Local → remote: new files and folders.
    let p = plan(&LocalFs, &src_s, &*fs, &rel, &quick);
    assert_eq!(p.summary.copy_to_right, 4, "{name}");
    run(Root::Local(&src), remote, &p, name);
    assert!(
        plan(&LocalFs, &src_s, &*fs, &rel, &by_content).ops.is_empty(),
        "{name}: in sync after upload"
    );
    if fs.caps().set_modified {
        let again = plan(&LocalFs, &src_s, &*fs, &rel, &quick);
        assert!(again.ops.is_empty(), "{name}: timestamps carried over: {:?}", again.ops);
    }

    // Edits and a deletion: replaced and deleted on the remote.
    write_local(&src, "a.txt", b"one, edited");
    std::fs::remove_file(src.join("gone.txt")).unwrap();
    let p = plan(&LocalFs, &src_s, &*fs, &rel, &by_content);
    let kinds: Vec<String> = p.ops.iter().map(|o| format!("{:?} {}", o.kind, o.rel_path)).collect();
    assert_eq!(kinds, ["Copy a.txt", "Delete gone.txt"], "{name}");
    run(Root::Local(&src), remote, &p, name);
    assert_eq!(
        std::fs::read_to_string(m.local_copy(&format!("{uri}/a.txt"), true).unwrap()).unwrap(),
        "one, edited"
    );
    let names: Vec<String> = m.list(&uri).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["sub", "a.txt"], "{name}: no temp files, gone.txt deleted");

    // Remote → local.
    let p = plan(&*fs, &rel, &LocalFs, &back_s, &quick);
    run(remote, Root::Local(&back), &p, name);
    assert_eq!(std::fs::read(back.join("sub/deeper/big.bin")).unwrap(), big, "{name}");
    assert_eq!(
        std::fs::read_to_string(back.join("a.txt")).unwrap(),
        "one, edited",
        "{name}"
    );

    // Dropped connections: the download resumes with a ranged read; the
    // upload resumes where the server supports offset writes.
    let flaky = Flaky::new(fs.clone(), 300_000);
    let shaky = Root::Remote {
        fs: &flaky,
        path: &rel,
        label: &uri,
    };
    let p = plan(&*fs, &rel, &LocalFs, &resumed_s, &quick);
    let only_big = SyncPlan {
        ops: p
            .ops
            .iter()
            .filter(|o| o.rel_path.ends_with("big.bin") || o.is_dir)
            .cloned()
            .collect(),
        ..p
    };
    let r = run(shaky, Root::Local(&resumed), &only_big, name);
    assert!(r.results.iter().any(|x| x.attempts == 2), "{name}: retried");
    assert_eq!(
        std::fs::read(resumed.join("sub/deeper/big.bin")).unwrap(),
        big,
        "{name}: resumed download intact"
    );
    assert_eq!(
        *flaky.read_offsets.lock().unwrap(),
        [0, 300_000],
        "{name}: download resumed at the cut"
    );

    write_local(&src, "sub/deeper/big2.bin", &big);
    let p = plan(&LocalFs, &src_s, &*fs, &rel, &by_content);
    assert_eq!(p.ops.len(), 1, "{name}: {:?}", p.ops);
    run(Root::Local(&src), shaky, &p, name);
    let resumable = fs.caps().resume_write && fs.caps().rename;
    let expected: &[u64] = if resumable { &[0, 300_000] } else { &[0, 0] };
    assert_eq!(
        *flaky.write_offsets.lock().unwrap(),
        expected,
        "{name}: upload resumed or restarted"
    );
    let got = std::fs::read(m.local_copy(&format!("{uri}/sub/deeper/big2.bin"), true).unwrap()).unwrap();
    assert!(got == big, "{name}: uploaded file intact after the drop");

    m.delete(&uri).unwrap();
    assert!(
        m.list(&uri).map(|e| e.is_empty()).unwrap_or(true),
        "{name}: sync scratch removed"
    );
}
