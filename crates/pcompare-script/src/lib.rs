//! PCompare's script processor (`pcompare @script.txt`), headless quick
//! compare (`pcompare /qc left right`) and headless merge (`/automerge`),
//! compatible with the common compare-script language, switches and exit
//! codes.
//!
//! ```text
//! # Mirror a project to a backup, then report what differed before.
//! log verbose append:"%TEMP%\backup %date%.log"
//! criteria timestamp:2sec size
//! filter "-target\;-.git\;-*.tmp"
//! load "%1" "%2"
//! expand all
//! folder-report layout:side-by-side options:display-mismatches &
//!    output-to:"%2\..\before.html" output-options:html-color
//! sync create-empty mirror:left->right
//! ```
//!
//! Parsing ([`parse`]) checks the whole script before anything runs; the
//! interpreter ([`interp`]) loads folders through a [`host::Host`] (local
//! folders, archives, snapshots and, from the app, remote profiles and saved
//! sessions) and uses pcompare-sync's executors for every file operation.

pub mod console;
pub mod host;
pub mod interp;
pub mod merge;
pub mod parse;
pub mod quick;
mod report;

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

/// Exit code: every command succeeded.
pub const EXIT_OK: i32 = 0;
/// Exit code ("unknown error"): the script ran but some
/// commands failed.
pub const EXIT_ERRORS: i32 = interp::EXIT_FAILED;
/// Exit code: the script file couldn't be read.
pub const EXIT_UNREADABLE: i32 = 105;
/// Exit code: the script has syntax errors (nothing ran).
pub const EXIT_INVALID: i32 = 106;
/// Exit code: a `load` failed, which ends the script.
pub const EXIT_LOAD_FAILED: i32 = interp::EXIT_LOAD_FAILED;

/// Asks a yes/no question on the terminal; "no" without one.
fn ask_terminal(question: &str) -> bool {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return false;
    }
    let mut err = std::io::stderr();
    let _ = write!(err, "{question} [y/N] ");
    let _ = err.flush();
    let mut answer = String::new();
    if stdin.lock().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// How a script file runs.
pub struct RunOptions<'a> {
    /// `%1`..`%9`.
    pub params: &'a [String],
    /// `/silent`: only errors are printed.
    pub silent: bool,
    /// List file operations instead of performing them.
    pub dry_run: bool,
    pub host: &'a dyn host::Host,
}

/// Runs a script file, printing progress to stdout (unless silent) and
/// errors to stderr. Returns the standard exit code: 0 success, 100
/// some commands failed, 105 unreadable script, 106 syntax errors, 107 a
/// `load` failed.
pub fn run_file(path: &Path, opts: &RunOptions) -> i32 {
    let mut stderr = std::io::stderr();
    let text = match std::fs::read(path) {
        Ok(bytes) => pcompare_core::textfile::decode_auto(&bytes).text,
        Err(e) => {
            let _ = writeln!(stderr, "error: cannot read script {}: {e}", path.display());
            return EXIT_UNREADABLE;
        }
    };
    let env_var = |name: &str| std::env::var(name).ok();
    let stmts = match parse::parse_script(&text, opts.params, &env_var) {
        Ok(s) => s,
        Err(errors) => {
            for e in errors {
                let _ = writeln!(stderr, "error: {}: {e}", path.display());
            }
            return EXIT_INVALID;
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut stdout = std::io::stdout();
    let mut confirm = ask_terminal;
    interp::run(
        &stmts,
        &mut interp::Env {
            silent: opts.silent,
            dry_run: opts.dry_run,
            cwd,
            out: &mut stdout,
            err: &mut stderr,
            confirm: &mut confirm,
            host: opts.host,
        },
    )
    .exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classic sample scripts.
    pub(crate) const SAMPLE_SCRIPTS: &[&str] = &[
        "load \"Sync Laptop\"\nsync update:left->right\n",
        "# Turn logging on.\nlog normal \"C:\\My Log.txt\"\n# Load the default session and exclude all but certain file types.\nload <default>\nfilter \"*.html;*.htm;*.png;*.jpg\"\n# Load the base folders.\nload \"C:\\My Folder\" ftp://user@website.com/folder\n# Copy different files left to right, delete orphans on right.\nsync mirror:left->right\n",
        "file-report layout:side-by-side &\n options:display-mismatches &\n output-to:\"%3\" \"%1\" \"%2\"\n",
        "text-report layout:side-by-side &\n options:ignore-unimportant,display-context &\n output-to:\"%3\" output-options:html-color \"%1\" \"%2\"\n",
        "# Turn verbose logging on.\nlog verbose append:\"C:\\My Log.txt\"\n# Set the comparison criteria.\ncriteria timestamp size\n# Load source and target folders.\nload \"C:\\My Folder\" ftp://user@website.com/folder\n# Filter to only include source files, ignore CVS subfolders.\nfilter \"*.htm;*.html;*.php;*.jpg;*.gif;-CVS\\\"\n# Sync the local files to the web site, creating empty folders.\nsync create-empty mirror:left->right\n",
        "# Log to a file of the form \"C:\\My Log 2012-12-31.txt\".\n# The exact format depends on your regional and language settings.\nlog verbose append:\"C:\\My Log %date%.txt\"\n# Compare the text contents of the files.\ncriteria rules-based\n# Only compare Delphi source files, ignore the archive folder.\nfilter \"*.pas;*.dfm;*.dpr;*.inc;-archive\\\"\nload \"C:\\My Folder\" \"C:\\Your Folder\"\nfolder-report layout:summary &\n options:display-mismatches,column-attributes,column-timestamp,column-size &\n output-to:\"My Report.txt\"\n",
        "option confirm:yes-to-all\n# Compare using crc.\ncriteria crc\n# Load current files against snapshot from the last patch.\nload \"C:\\My Folder\" \"C:\\My Snapshot.bcss\"\n# Expand to affect subfolders and files.\nexpand all\n# Select updated files.\nselect left.diff.files left.orphan.files\n# Copy changes to an archive file.\ncopyto left path:relative \"C:\\My Archive.zip\"\n# Update the snapshot.\nsnapshot save-crc left output:\"C:\\My Snapshot.bcss\"\n",
        "# Set up basic comparison features.\ncriteria timestamp:2sec\n# Filter out log files.\nfilter \"-*.log\"\n# Load first comparison.\nload \"C:\\My Folder\" \"C:\\Your Folder\"\n# Compare files with timestamp differences.\nselect newer.files older.files\n# Generate a report of the differences.\nfolder-report layout:summary options:display-mismatches output-to:\"C:\\My Report.txt\"\n",
    ];


    /// A host where the sample `ftp://user@website.com/folder` is a local
    /// folder.
    struct SampleHost {
        local: host::LocalHost,
        ftp: std::path::PathBuf,
    }

    const FTP: &str = "ftp://user@website.com/folder";

    impl host::Host for SampleHost {
        fn kind(&self, path: &str) -> pcompare_core::PathKind {
            if path == FTP {
                pcompare_core::PathKind::Directory
            } else {
                self.local.kind(path)
            }
        }
        fn folder(&self, path: &str) -> Result<host::Side, String> {
            if path == FTP {
                let mut side = host::Side::local(&self.ftp);
                side.shown = FTP.into();
                Ok(side)
            } else {
                self.local.folder(path)
            }
        }
        fn create_folder(&self, path: &str) -> Result<(), String> {
            self.local.create_folder(path)
        }
        fn local_file(&self, path: &str) -> Result<std::path::PathBuf, String> {
            self.local.local_file(path)
        }
        fn session(&self, name: &str) -> Option<pcompare_core::session::SavedSession> {
            self.local.session(name)
        }
        fn file_formats(&self) -> pcompare_convert::ConvertConfig {
            self.local.file_formats()
        }
        fn archives(&self) -> &std::sync::Arc<pcompare_archive::Archives> {
            self.local.archives()
        }
    }

    /// Folders and files the samples name, under `root` (for `C:\`).
    fn sample_world(root: &Path) -> SampleHost {
        let w = |p: &str, s: &str| {
            let path = root.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, s).unwrap();
        };
        w("My Folder/index.html", "<p>new</p>\n");
        w("My Folder/unit.pas", "begin end.\n");
        w("My Folder/sub/logo.png", "png");
        w("My Folder/CVS/Entries", "cvs");
        w("My Folder/archive/old.pas", "old");
        w("My Folder/debug.log", "log");
        w("Your Folder/index.html", "<p>old</p>\n");
        w("Your Folder/unit.pas", "begin end.\n");
        w("ftp/index.html", "<p>live</p>\n");
        w("ftp/stale.php", "<?php ?>\n");
        w("My File.txt", "one\ntwo\n");
        w("Your File.txt", "one\nTWO\n");
        let store = pcompare_core::session::SessionStore::new(root.join("sessions.json"));
        store
            .save(pcompare_core::session::SessionInput {
                id: None,
                name: "Sync Laptop".into(),
                kind: "folder".into(),
                left: root.join("My Folder").to_string_lossy().into(),
                right: root.join("Laptop").to_string_lossy().into(),
                base: None,
                output: None,
                settings: serde_json::Value::Null,
            })
            .unwrap();
        std::fs::create_dir_all(root.join("Laptop")).unwrap();
        SampleHost {
            local: host::LocalHost::temporary().with_sessions(store),
            ftp: root.join("ftp"),
        }
    }

    #[test]
    fn beyond_compare_sample_scripts_dry_run_and_run() {
        for dry_run in [true, false] {
            let d = tempfile::tempdir().unwrap();
            let root = d.path();
            let host = sample_world(root);
            // The snapshot sample compares against an earlier snapshot.
            let prep = interp::tests::run_with(
                root,
                "load \"Your Folder\"\nsnapshot save-crc left output:\"My Snapshot.bcss\"\n",
                &[],
                &host,
                false,
                false,
            );
            assert_eq!(prep.outcome.exit_code(), 0, "{}{}", prep.out, prep.err);
            let c_drive = format!("{}/", root.display());
            let params = ["My File.txt", "Your File.txt", "My Report.txt"].map(String::from);
            for (i, sample) in SAMPLE_SCRIPTS.iter().enumerate() {
                let script = sample.replace("C:\\", &c_drive);
                let r = interp::tests::run_with(root, &script, &params, &host, false, dry_run);
                assert_eq!(
                    r.outcome.exit_code(),
                    0,
                    "sample {i} (dry run {dry_run}):\n{}\n{}",
                    r.out,
                    r.err
                );
            }
            let laptop = root.join("Laptop/index.html").exists();
            let ftp_html = std::fs::read_to_string(root.join("ftp/index.html")).unwrap();
            if dry_run {
                assert!(!laptop && ftp_html == "<p>live</p>\n");
                assert!(!root.join("My Archive.zip").exists());
            } else {
                // Sync Laptop: update left->right.
                assert!(laptop);
                // The web-site mirror: copied, and the stale orphan deleted.
                assert_eq!(ftp_html, "<p>new</p>\n");
                assert!(!root.join("ftp/stale.php").exists());
                assert!(!root.join("ftp/CVS").exists());
                // Reports, logs, archive and refreshed snapshot.
                let log = std::fs::read_dir(root)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .find(|n| n.starts_with("My Log 2") && n.ends_with(".txt"));
                assert!(log.is_some(), "no dated log");
                let report = std::fs::read_to_string(root.join("My Report.txt")).unwrap();
                assert!(report.contains("Left orphan  sub/"), "{report}");
                assert!(root.join("My Archive.zip").exists());
                let snap = pcompare_archive::snapshot::load(&root.join("My Snapshot.bcss")).unwrap();
                assert!(snap.source.ends_with("My Folder"), "{}", snap.source);
            }
        }
    }

    fn opts<'a>(params: &'a [String], host: &'a host::LocalHost) -> RunOptions<'a> {
        RunOptions {
            params,
            silent: true,
            dry_run: false,
            host,
        }
    }

    #[test]
    fn missing_invalid_and_failing_scripts_exit_with_standard_codes() {
        let d = tempfile::tempdir().unwrap();
        let host = host::LocalHost::temporary();
        assert_eq!(
            run_file(&d.path().join("nope.txt"), &opts(&[], &host)),
            EXIT_UNREADABLE
        );
        let bad = d.path().join("bad.txt");
        std::fs::write(&bad, "load a b c\n").unwrap();
        assert_eq!(run_file(&bad, &opts(&[], &host), ), EXIT_INVALID);
        let missing = d.path().join("missing.txt");
        std::fs::write(&missing, "load /no/such/left /no/such/right\n").unwrap();
        assert_eq!(run_file(&missing, &opts(&[], &host)), EXIT_LOAD_FAILED);
        let failing = d.path().join("failing.txt");
        std::fs::write(&failing, "select all\n").unwrap();
        assert_eq!(run_file(&failing, &opts(&[], &host)), EXIT_ERRORS);
    }

    #[test]
    fn parameters_reach_the_script() {
        let d = tempfile::tempdir().unwrap();
        for side in ["a", "b"] {
            std::fs::create_dir_all(d.path().join(side)).unwrap();
            std::fs::write(d.path().join(side).join("f.txt"), side).unwrap();
        }
        let script = d.path().join("s.txt");
        let out = d.path().join("r.csv");
        std::fs::write(
            &script,
            "load \"%1\" \"%2\"\nfolder-report layout:summary output-to:\"%3\"\n",
        )
        .unwrap();
        let params = [d.path().join("a"), d.path().join("b"), out.clone()]
            .map(|p| p.to_string_lossy().into_owned());
        let host = host::LocalHost::temporary();
        assert_eq!(run_file(&script, &opts(&params, &host)), EXIT_OK);
        let csv = std::fs::read_to_string(&out).unwrap();
        assert!(
            csv.starts_with("Status,Path\r\n") && csv.ends_with(",f.txt\r\n"),
            "{csv}"
        );
    }
}
