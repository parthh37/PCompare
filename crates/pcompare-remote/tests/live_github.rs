//! Live reads from github.com. Skipped unless `PCOMPARE_LIVE_GITHUB=1`;
//! signs in through the GitHub CLI (`gh auth login`), and checks the
//! anonymous path too. Only reads public repositories.

use pcompare_core::folder::{compare_folders_with, ContentMethod, FolderCompareOptions};
use pcompare_remote::profile::{MemorySecrets, ProfileInput};
use pcompare_remote::provider::ProviderKind;
use pcompare_remote::RemoteManager;

fn profile(name: &str, settings: &[(&str, &str)]) -> ProfileInput {
    ProfileInput {
        id: None,
        name: name.into(),
        kind: ProviderKind::Github,
        settings: settings.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        secrets: Default::default(),
    }
}

#[test]
fn live_github() {
    if std::env::var("PCOMPARE_LIVE_GITHUB").as_deref() != Ok("1") {
        eprintln!("PCOMPARE_LIVE_GITHUB not set; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let m = RemoteManager::new(dir.path(), dir.path().join("cache"), Box::<MemorySecrets>::default());

    // Signed in through gh: the top level lists owners.
    let cli = m.profiles.save(profile("gh", &[("auth", "github-cli")])).unwrap();
    eprintln!("{}", m.test(&profile("gh", &[("auth", "github-cli")])).unwrap());
    let base = format!("remote://{}", cli.id);

    // Branches, with a slash encoded, plus tags.
    let refs: Vec<String> = m.list(&format!("{base}/tauri-apps/tauri")).unwrap().into_iter().map(|e| e.name).collect();
    assert!(refs.iter().any(|r| r == "dev"), "{refs:?}");
    let slashed = refs.iter().find(|r| r.contains("%2F")).expect("a branch with a slash").clone();
    assert!(!m.list(&format!("{base}/tauri-apps/tauri/{slashed}")).unwrap().is_empty());

    // Two branches of a small repository, compared byte for byte.
    let hello = format!("{base}/octocat/Hello-World");
    let opts = FolderCompareOptions { content: ContentMethod::Binary, ..Default::default() };
    let (lfs, l) = m.target(&format!("{hello}/master")).unwrap();
    let (rfs, r) = m.target(&format!("{hello}/test")).unwrap();
    let res = compare_folders_with(&*lfs, &l, &*rfs, &r, &opts).unwrap();
    eprintln!("Hello-World master vs test: {:?}", res.stats);
    assert!(res.stats.same + res.stats.different + res.stats.left_only + res.stats.right_only > 0);

    let readme = m.local_copy(&format!("{hello}/master/README"), true).unwrap();
    assert!(std::fs::read_to_string(readme).unwrap().contains("Hello World"));

    // A missing ref or file is "not found", not a crash.
    assert!(m.list(&format!("{hello}/no-such-branch-xyz")).is_err());
    assert!(m.local_copy(&format!("{hello}/master/nope.txt"), true).is_err());

    // Writes are refused.
    assert!(m.create_dir(&format!("{hello}/master/new")).is_err());

    // Anonymous, pinned to one repository: paths start at its refs.
    let anon = m
        .profiles
        .save(profile("anon", &[("auth", "anonymous"), ("owner", "octocat"), ("repo", "Hello-World")]))
        .unwrap();
    let names: Vec<String> = m.list(&format!("remote://{}/master", anon.id)).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["README"]);
}
