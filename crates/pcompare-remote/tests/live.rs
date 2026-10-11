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

use pcompare_core::folder::{compare_folders_with, CompareCriteria, FolderCompareOptions};
use pcompare_remote::profile::{MemorySecrets, ProfileInput};
use pcompare_remote::RemoteManager;
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    profile: ProfileInput,
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

    for case in cases {
        let name = case.profile.name.clone();
        let msg = m.test(&case.profile).unwrap_or_else(|e| panic!("{name}: test failed: {e}"));
        eprintln!("{name}: {msg}");
        let p = m.profiles.save(case.profile).unwrap();
        let base = format!("remote://{}", p.id);

        let names: Vec<String> = m.list(&format!("{base}/left")).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["sub", "a.txt", "orphan.txt"], "{name}: folders first, then by name");

        let opts = FolderCompareOptions {
            criteria: CompareCriteria::Content,
            ..Default::default()
        };
        let (lfs, l) = m.target(&format!("{base}/left")).unwrap();
        let (rfs, r) = m.target(&format!("{base}/right")).unwrap();
        let res = compare_folders_with(&*lfs, &l, &*rfs, &r, &opts).unwrap();
        assert_eq!((res.stats.same, res.stats.different, res.stats.left_only), (1, 1, 1), "{name}");

        let copy = m.local_copy(&format!("{base}/right/sub/b.txt"), true).unwrap();
        assert_eq!(std::fs::read_to_string(copy).unwrap(), "two!\n", "{name}");

        // SFTP pins the host key on first real use.
        if p.kind == pcompare_remote::provider::ProviderKind::Sftp {
            assert!(m.profiles.get(&p.id).unwrap().settings.contains_key("host_key"), "{name}: host key pinned");
            // A different key afterwards must be refused.
            m.profiles.set_setting(&p.id, "host_key", "SHA256:not-the-real-key").unwrap();
            m.invalidate(&p.id);
            let err = m.list(&format!("{base}/left")).expect_err("tampered host key must fail").to_string();
            assert!(err.contains("HOST KEY CHANGED"), "{name}: {err}");
        }
        eprintln!("{name}: ok");
    }
}
