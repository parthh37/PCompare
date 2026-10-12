//! Source-control integration (araxis:vcs): `--wait` for difftool /
//! mergetool launches, the Settings page that sets PCompare up as git's
//! diff and merge tool (with snippets for Mercurial, Perforce and
//! Subversion), and the revision picker behind `git:` / `hg:` sides.
//!
//! ## Waiting
//! A VCS runs `pcompare --wait <left> <right> …` and carries on when the
//! process exits — but with PCompare already open, a launch forwards its
//! arguments to the running window and exits at once. So a waiting launch
//! doesn't open anything itself: it starts a second copy of PCompare with
//! the same arguments plus `--wait-token=<token>` (that copy either becomes
//! the window or forwards to the running one), then polls a folder in the
//! temp directory:
//!
//! - the instance that opens the tab writes `<token>.open`, naming a lock
//!   file it holds for as long as it runs;
//! - closing the tab writes `<token>.done`;
//! - if the lock can be taken, that instance is gone (quit or crashed), which
//!   counts as closing the tab.
//!
//! Temp files work the same on Windows and macOS. The exit code is 0, or 1
//! for a merge whose output file was never saved (git's `trustExitCode`).

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pcompare_archive::vcs::{self, Revision, VcsKind};
use serde::Serialize;

use crate::{blocking, cli, CmdResult};

// ---------- waiting ----------

fn wait_dir() -> PathBuf {
    std::env::temp_dir().join("pcompare-wait")
}

/// Tokens are file names: keep them to letters, digits and `-`.
fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 64
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn token_file(token: &str, ext: &str) -> PathBuf {
    wait_dir().join(format!("{token}.{ext}"))
}

fn is_wait_switch(arg: &str) -> bool {
    let a = arg.to_ascii_lowercase();
    a == "--wait" || a == "-wait" || a == "/wait"
}

/// What a merge output looked like before the tool ran.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp(Option<(u64, Option<SystemTime>)>);

impl Stamp {
    fn of(path: &Path) -> Self {
        Stamp(
            fs::metadata(path)
                .ok()
                .map(|m| (m.len(), m.modified().ok())),
        )
    }
}

/// The merge output a launch names (relative to the current directory).
fn merge_output(args: &[String]) -> Option<PathBuf> {
    let opts = cli::resolve(cli::gui_options(args), None);
    opts.paths.get(3).map(PathBuf::from)
}

/// The exit code once the tab is closed: merges must have saved their
/// output; comparisons always succeed.
fn exit_code(output: Option<(&Path, &Stamp)>) -> i32 {
    match output {
        Some((path, before)) if Stamp::of(path) == *before => 1,
        _ => 0,
    }
}

/// Handles `--wait` (and `/mergeoutput=`, which implies it) before any
/// window exists: relaunches PCompare with a token and blocks until the tab
/// opened for it is closed. `None` when this launch doesn't wait.
pub fn run_wait_mode(args: &[String]) -> Option<i32> {
    let opts = cli::gui_options(args);
    if opts.wait_token.is_some() || !(opts.wait || opts.merge_output.is_some()) {
        return None;
    }
    if opts.paths.is_empty() && opts.compare_to.is_none() {
        // Nothing to wait for: just open the window.
        return None;
    }
    let relaunch = std::env::current_exe().map(Command::new);
    Some(match relaunch.and_then(|cmd| wait_for_tab(cmd, args)) {
        Ok(code) => code,
        Err(e) => {
            pcompare_script::console::attach_parent();
            eprintln!("pcompare --wait: {e}");
            2
        }
    })
}

fn new_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{:x}-{:x}", std::process::id(), nanos)
}

/// Runs `relaunch` (PCompare itself) with `args` and a fresh token, then
/// waits for the handshake.
fn wait_for_tab(mut relaunch: Command, args: &[String]) -> io::Result<i32> {
    fs::create_dir_all(wait_dir())?;
    let token = new_token();
    let output = merge_output(args).map(|p| {
        let stamp = Stamp::of(&p);
        (p, stamp)
    });
    let mut child = relaunch
        .args(args.iter().filter(|a| !is_wait_switch(a)))
        .arg(format!("--wait-token={token}"))
        .spawn()?;
    let (open, done) = (token_file(&token, "open"), token_file(&token, "done"));
    let cleanup = || {
        let _ = fs::remove_file(&open);
        let _ = fs::remove_file(&done);
    };
    let code = || exit_code(output.as_ref().map(|(p, s)| (p.as_path(), s)));
    let mut child_exit: Option<Instant> = None;
    loop {
        if done.exists() {
            cleanup();
            return Ok(code());
        }
        if let Ok(lock) = fs::read_to_string(&open) {
            if instance_gone(Path::new(lock.trim())) {
                cleanup();
                return Ok(code());
            }
        } else if child_exit.is_none() {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    cleanup();
                    return Ok(status.code().unwrap_or(2));
                }
                child_exit = Some(Instant::now());
            }
        } else if child_exit.is_some_and(|t| t.elapsed() > Duration::from_secs(60)) {
            // Forwarded, but no window ever took the comparison.
            cleanup();
            return Err(io::Error::other("PCompare didn't open the comparison"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Whether the instance holding `lock` has exited (its lock is free).
fn instance_gone(lock: &Path) -> bool {
    match File::open(lock) {
        Ok(f) => f.try_lock().is_ok(),
        // The lock file is removed only with the wait folder: treat as gone.
        Err(_) => true,
    }
}

/// This instance's lock file, created and locked on first use and held
/// until the process exits.
fn instance_lock() -> io::Result<PathBuf> {
    // A mutex, not a OnceLock: two threads creating the file at once would
    // block on each other's lock.
    static LOCK: Mutex<Option<(PathBuf, File)>> = Mutex::new(None);
    let mut slot = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((p, _)) = slot.as_ref() {
        return Ok(p.clone());
    }
    fs::create_dir_all(wait_dir())?;
    // Lock files of instances that have exited.
    for entry in fs::read_dir(wait_dir())?.flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("instance-") && name.ends_with(".lock") && instance_gone(&p) {
            let _ = fs::remove_file(&p);
        }
    }
    // Locked before it gets its `.lock` name, so the sweep above in another
    // instance never sees it unlocked.
    let path = wait_dir().join(format!("instance-{}.lock", std::process::id()));
    let tmp = path.with_extension("new");
    let file = File::create(&tmp)?;
    file.lock()?;
    fs::rename(&tmp, &path)?;
    *slot = Some((path.clone(), file));
    Ok(path)
}

/// Called where a launch is received: takes responsibility for a waited
/// launch by telling its waiting process which instance holds the tab.
pub fn register(token: &str) {
    if !valid_token(token) {
        return;
    }
    if let Err(e) = instance_lock()
        .and_then(|lock| fs::write(token_file(token, "open"), lock.to_string_lossy().as_bytes()))
    {
        eprintln!("--wait: couldn't register {token}: {e}");
    }
}

/// The tab opened for a waited launch was closed.
#[tauri::command]
pub fn vcs_wait_done(token: String) -> CmdResult<()> {
    if !valid_token(&token) {
        return Err("invalid wait token".into());
    }
    fs::create_dir_all(wait_dir()).map_err(|e| e.to_string())?;
    fs::write(token_file(&token, "done"), b"closed").map_err(|e| e.to_string())
}

// ---------- git setup ----------

/// The PCompare executable as tools should call it (forward slashes on
/// Windows, which Git's shell and Mercurial both accept).
fn exe_for_tools() -> String {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "pcompare".into());
    if cfg!(windows) {
        exe.replace('\\', "/")
    } else {
        exe
    }
}

/// One `git config --global` setting.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitSetting {
    pub key: String,
    pub value: String,
    /// The same setting as a shell command, for display and copying.
    pub command: String,
    /// What git has for `key` now.
    pub current: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitSetup {
    pub git_available: bool,
    pub settings: Vec<GitSetting>,
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn git_settings(exe: &str, make_default: bool) -> Vec<(String, String)> {
    let mut s = vec![
        (
            "difftool.pcompare.cmd".to_owned(),
            format!(r#""{exe}" --wait "$LOCAL" "$REMOTE""#),
        ),
        (
            "mergetool.pcompare.cmd".to_owned(),
            format!(r#""{exe}" --wait "$LOCAL" "$REMOTE" "$BASE" "$MERGED""#),
        ),
        (
            "mergetool.pcompare.trustExitCode".to_owned(),
            "true".to_owned(),
        ),
    ];
    if make_default {
        s.push(("diff.tool".to_owned(), "pcompare".to_owned()));
        s.push(("merge.tool".to_owned(), "pcompare".to_owned()));
    }
    s
}

fn git_command() -> Command {
    let mut cmd = Command::new("git");
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn git_get(key: &str) -> Option<String> {
    let out = git_command()
        .args(["config", "--global", "--get", key])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn git_setup(make_default: bool) -> GitSetup {
    let available = vcs::available(VcsKind::Git);
    let settings = git_settings(&exe_for_tools(), make_default)
        .into_iter()
        .map(|(key, value)| GitSetting {
            command: format!("git config --global {key} {}", shell_quote(&value)),
            current: available.then(|| git_get(&key)).flatten(),
            key,
            value,
        })
        .collect();
    GitSetup {
        git_available: available,
        settings,
    }
}

/// The settings "Set up git" would write, and what git has now.
#[tauri::command]
pub async fn vcs_git_setup_preview(make_default: bool) -> CmdResult<GitSetup> {
    blocking(move || Ok(git_setup(make_default))).await
}

/// Writes the settings shown by [`vcs_git_setup_preview`] (computed here
/// again, so nothing but these keys can be written).
#[tauri::command]
pub async fn vcs_git_setup_apply(make_default: bool) -> CmdResult<GitSetup> {
    blocking(move || {
        for (key, value) in git_settings(&exe_for_tools(), make_default) {
            let out = git_command()
                .args(["config", "--global", &key, &value])
                .output()
                .map_err(|e| pcompare_core::CoreError::Other(format!("couldn't run git: {e}")))?;
            if !out.status.success() {
                return Err(pcompare_core::CoreError::Other(format!(
                    "git config {key} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
        }
        Ok(git_setup(make_default))
    })
    .await
}

// ---------- other tools ----------

/// Copy-paste configuration for a tool PCompare doesn't configure itself.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    pub tool: String,
    /// Where it goes.
    pub target: String,
    pub text: String,
}

fn snippets(exe: &str) -> Vec<Snippet> {
    let windows = cfg!(windows);
    let hgrc = if windows {
        "%USERPROFILE%\\mercurial.ini"
    } else {
        "~/.hgrc"
    };
    let mut out = vec![
        Snippet {
            tool: "Mercurial".into(),
            target: format!("{hgrc} — then `hg pcompare` for a diff; `hg merge` / `hg resolve` use it"),
            text: format!(
                "[extensions]\nextdiff =\n\n[extdiff]\ncmd.pcompare = {exe}\nopts.pcompare = --wait\n\n\
[merge-tools]\npcompare.executable = {exe}\npcompare.args = --wait $local $other $base $output\n\
pcompare.gui = True\npcompare.premerge = keep\npcompare.binary = True\npcompare.priority = 1\n\n\
[ui]\nmerge = pcompare\n"
            ),
        },
        Snippet {
            tool: "Perforce (p4)".into(),
            target: if windows {
                "Command Prompt (stored in the registry by p4 set)".into()
            } else {
                "your shell profile".into()
            },
            text: if windows {
                format!(
                    "p4 set P4DIFF=\"\\\"{exe}\\\" --wait\"\np4 set P4MERGE=\"\\\"{exe}\\\" --wait --base-first\"\n"
                )
            } else {
                format!(
                    "export P4DIFF='\"{exe}\" --wait'\nexport P4MERGE='\"{exe}\" --wait --base-first'\n"
                )
            },
        },
        Snippet {
            tool: "Perforce (P4V)".into(),
            target: "P4V → Preferences → Diff / Merge → Other application".into(),
            text: format!(
                "Location:        {exe}\nDiff arguments:  --wait %1 %2\nMerge arguments: --wait %2 %1 %b %r\n"
            ),
        },
        Snippet {
            tool: "Subversion (diff)".into(),
            target: "the command line".into(),
            text: format!("svn diff --diff-cmd \"{exe}\" -x --wait\n"),
        },
    ];
    out.push(if windows {
        Snippet {
            tool: "Subversion (merge)".into(),
            target: "save as pcompare-svn-merge.cmd, then in %APPDATA%\\Subversion\\config: [helpers] merge-tool-cmd = <path to it>".into(),
            text: format!("@\"{exe}\" --wait --base-first %1 %2 %3 %4\n"),
        }
    } else {
        Snippet {
            tool: "Subversion (merge)".into(),
            target: "save as pcompare-svn-merge (chmod +x), then in ~/.subversion/config: [helpers] merge-tool-cmd = <path to it>".into(),
            text: format!("#!/bin/sh\nexec \"{exe}\" --wait --base-first \"$1\" \"$2\" \"$3\" \"$4\"\n"),
        }
    });
    out
}

#[tauri::command]
pub fn vcs_snippets() -> Vec<Snippet> {
    snippets(&exe_for_tools())
}

// ---------- revision picker ----------

/// Which client a working-tree path belongs to, and whether it's installed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoInfo {
    pub kind: Option<VcsKind>,
    pub git_available: bool,
    pub hg_available: bool,
}

#[tauri::command]
pub async fn vcs_detect(path: String) -> CmdResult<RepoInfo> {
    blocking(move || {
        Ok(RepoInfo {
            kind: vcs::detect(&path),
            git_available: vcs::available(VcsKind::Git),
            hg_available: vcs::available(VcsKind::Hg),
        })
    })
    .await
}

/// The latest commits touching a working-tree file or folder.
#[tauri::command]
pub async fn vcs_revisions(
    kind: VcsKind,
    path: String,
    limit: Option<usize>,
) -> CmdResult<Vec<Revision>> {
    blocking(move || vcs::recent_revisions(kind, &path, limit.unwrap_or(50).min(500))).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_waiting_launches_wait() {
        // These return before spawning anything.
        assert_eq!(run_wait_mode(&args(&["a", "b"])), None);
        assert_eq!(run_wait_mode(&args(&["--wait"])), None);
        assert_eq!(
            run_wait_mode(&args(&["--wait", "--wait-token=x", "a", "b"])),
            None
        );
        assert_eq!(
            run_wait_mode(&args(&["l", "r", "b", "/mergeoutput=o", "--wait-token=x"])),
            None
        );
        assert!(
            is_wait_switch("/WAIT") && is_wait_switch("--wait") && !is_wait_switch("--waiting")
        );
    }

    #[test]
    fn tokens_are_safe_file_names() {
        assert!(valid_token(&new_token()));
        assert!(!valid_token("../x"));
        assert!(!valid_token(""));
        assert!(vcs_wait_done("a/b".into()).is_err());
    }

    #[test]
    fn merge_output_and_exit_codes() {
        let dir = std::env::temp_dir().join(format!("pcompare-vcs-test-{}", new_token()));
        fs::create_dir_all(&dir).unwrap();
        let out = dir.join("merged.txt");
        fs::write(&out, "<<<<<<<\n").unwrap();
        let o = out.to_string_lossy().into_owned();
        assert_eq!(
            merge_output(&args(&["--wait", "l", "r", "b", &o])),
            Some(out.clone())
        );
        assert_eq!(merge_output(&args(&["--wait", "l", "r"])), None);
        let before = Stamp::of(&out);
        assert_eq!(exit_code(Some((&out, &before))), 1);
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&out, "resolved\n").unwrap();
        assert_eq!(exit_code(Some((&out, &before))), 0);
        assert_eq!(exit_code(None), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_handshake_files() {
        let token = new_token();
        register(&token);
        let lock = fs::read_to_string(token_file(&token, "open")).unwrap();
        // This process holds the lock, so the instance isn't gone.
        assert!(!instance_gone(Path::new(lock.trim())));
        assert!(instance_gone(&wait_dir().join("missing.lock")));
        vcs_wait_done(token.clone()).unwrap();
        assert!(token_file(&token, "done").exists());
        let _ = fs::remove_file(token_file(&token, "open"));
        let _ = fs::remove_file(token_file(&token, "done"));
    }

    /// A stand-in for the relaunched PCompare: a shell script that gets the
    /// token as its last argument.
    #[cfg(unix)]
    fn fake(script: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        let wait = wait_dir().to_string_lossy().into_owned();
        cmd.args([
            "-c",
            &format!("for t; do :; done; tok=${{t#--wait-token=}}; W='{wait}'; {script}"),
            "sh",
        ]);
        cmd
    }

    #[cfg(unix)]
    #[test]
    fn waits_for_the_handshake() {
        // The tab is opened, then closed.
        // (This process holds its instance lock, as the window would.)
        let held = instance_lock().unwrap().display().to_string();
        let s = fake(&format!(
            r#"echo '{held}' > "$W/$tok.open"; sleep 0.3; touch "$W/$tok.done""#
        ));
        let started = Instant::now();
        assert_eq!(wait_for_tab(s, &args(&["--wait", "a", "b"])).unwrap(), 0);
        assert!(started.elapsed() >= Duration::from_millis(250));
        // The forwarding copy fails: its exit code is passed on.
        assert_eq!(wait_for_tab(fake("exit 3"), &args(&["a", "b"])).unwrap(), 3);
        // A merge closed without saving its output exits 1; saving it, 0.
        let dir = std::env::temp_dir().join(format!("pcompare-vcs-test-{}", new_token()));
        fs::create_dir_all(&dir).unwrap();
        let out = dir.join("merged.txt");
        fs::write(&out, "conflict\n").unwrap();
        let o = out.to_string_lossy().into_owned();
        let merge = args(&["--wait", "l", "r", "b", &o]);
        let unsaved = fake(r#"touch "$W/$tok.done""#);
        assert_eq!(wait_for_tab(unsaved, &merge).unwrap(), 1);
        let saved = fake(&format!(
            r#"sleep 0.05; echo ok > '{o}'; touch "$W/$tok.done""#
        ));
        assert_eq!(wait_for_tab(saved, &merge).unwrap(), 0);
        // The instance holding the tab quits: its lock is free.
        let lock = dir.join("instance.lock");
        fs::write(&lock, "").unwrap();
        let quit = fake(&format!(r#"echo '{}' > "$W/$tok.open""#, lock.display()));
        assert_eq!(wait_for_tab(quit, &merge).unwrap(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_settings_call_back_with_wait() {
        let s = git_settings("/Apps/P Compare/pcompare", true);
        let get = |k: &str| s.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(
            get("mergetool.pcompare.cmd"),
            Some(r#""/Apps/P Compare/pcompare" --wait "$LOCAL" "$REMOTE" "$BASE" "$MERGED""#)
        );
        assert_eq!(get("mergetool.pcompare.trustExitCode"), Some("true"));
        assert_eq!(get("merge.tool"), Some("pcompare"));
        assert_eq!(git_settings("x", false).len(), 3);
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        let sn = snippets("/x/pcompare");
        assert!(sn.iter().any(|s| s
            .text
            .contains("pcompare.args = --wait $local $other $base $output")));
        assert!(sn.iter().all(|s| s.text.contains("--wait")));
    }
}
