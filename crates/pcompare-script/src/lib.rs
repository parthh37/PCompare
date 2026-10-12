//! PCompare's script processor (`pcompare @script.txt`) and headless quick
//! compare (`pcompare /qc left right`), modelled on Beyond Compare's.
//!
//! ```text
//! # Mirror a project to a backup, then report what differed before.
//! log verbose "%TEMP%\backup.log"
//! criteria timestamp:2sec size
//! filter "-target\;-.git\;-*.tmp"
//! load "%1" "%2"
//! expand all
//! folder-report layout:side-by-side options:display-mismatches output-to:"%2\..\before.html"
//! sync create-empty mirror:left->right
//! ```
//!
//! Parsing ([`parse`]) checks the whole script before anything runs; the
//! interpreter ([`interp`]) works on local folders and uses pcompare-sync's
//! executor (atomic copies, recycle bin, retries) for every file operation.

pub mod console;
pub mod interp;
pub mod parse;
pub mod quick;

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

/// Exit code: every command succeeded.
pub const EXIT_OK: i32 = 0;
/// Exit code: the script ran but some commands failed.
pub const EXIT_ERRORS: i32 = 1;
/// Exit code: the script couldn't be read or has syntax errors (nothing ran).
pub const EXIT_INVALID: i32 = 2;

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

/// Runs a script file with `params` as `%1`..`%9`, printing progress to
/// stdout (unless `silent`) and errors to stderr. Returns the exit code.
pub fn run_file(path: &Path, params: &[String], silent: bool) -> i32 {
    let mut stderr = std::io::stderr();
    let text = match std::fs::read(path) {
        Ok(bytes) => pcompare_core::textfile::decode_auto(&bytes).text,
        Err(e) => {
            let _ = writeln!(stderr, "error: cannot read script {}: {e}", path.display());
            return EXIT_INVALID;
        }
    };
    let env_var = |name: &str| std::env::var(name).ok();
    let stmts = match parse::parse_script(&text, params, &env_var) {
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
    let outcome = interp::run(
        &stmts,
        &mut interp::Env {
            silent,
            cwd,
            out: &mut stdout,
            err: &mut stderr,
            confirm: &mut confirm,
        },
    );
    if outcome.exit_code() == 0 {
        EXIT_OK
    } else {
        EXIT_ERRORS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_invalid_scripts_exit_with_2() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            run_file(&d.path().join("nope.txt"), &[], true),
            EXIT_INVALID
        );
        let bad = d.path().join("bad.txt");
        std::fs::write(&bad, "load only-one\n").unwrap();
        assert_eq!(run_file(&bad, &[], true), EXIT_INVALID);
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
        assert_eq!(run_file(&script, &params, true), EXIT_OK);
        let csv = std::fs::read_to_string(&out).unwrap();
        assert!(
            csv.starts_with("Status,Path\r\n") && csv.ends_with(",f.txt\r\n"),
            "{csv}"
        );
    }
}
