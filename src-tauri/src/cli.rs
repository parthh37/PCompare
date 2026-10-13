//! Command-line interface, Beyond Compare–compatible where that makes sense.
//!
//! ```text
//! pcompare [options] <left> [<right> [<base> [<output>]]]
//! pcompare @script.txt [params…] [/silent]
//! pcompare /qc[=size|crc|binary|rules] <left> <right>
//! ```
//!
//! Switches may start with `/` (Windows style, matched only for the exact
//! names below so `/Users/…` stays a path) or `-`/`--`. Scripts, quick
//! compares and `--help` run without a window and exit with a status code;
//! everything else becomes [`LaunchOptions`] for the UI.

use pcompare_script::quick::{self, QuickMethod};
use serde::Serialize;
use std::io::Write;
use std::path::Path;

/// What the UI should open, from the command line or a second launch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchOptions {
    /// `left right [base [output]]`.
    pub paths: Vec<String>,
    /// Forced session kind (`/fv=` / `--viewer`), one of `SESSION_KINDS`.
    pub viewer: Option<String>,
    /// Tab labels instead of the file names (`/title1=`, `/title2=`).
    pub title_left: Option<String>,
    pub title_right: Option<String>,
    /// Sides that must not be edited (`/readonly`, `/lro`, `/rro`).
    pub read_only_left: bool,
    pub read_only_right: bool,
    /// Where a merge writes its result (`/mergeoutput=`).
    pub merge_output: Option<String>,
    /// `--compare-to <path>`: pair with the remembered left side.
    #[serde(skip)]
    pub compare_to: Option<String>,
    // araxis:vcs
    /// `--wait` / `/wait`: block until the tab closes (vcs.rs).
    #[serde(skip)]
    pub wait: bool,
    /// Set by the waiting process on the launch it forwards; the tab reports
    /// back under it when it closes.
    pub wait_token: Option<String>,
    /// `--base-first`: paths come as `base theirs yours merged` (P4MERGE).
    #[serde(skip)]
    pub base_first: bool,
    // araxis:vcs end
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Help,
    Script {
        file: String,
        params: Vec<String>,
        silent: bool,
    },
    QuickCompare {
        left: String,
        right: String,
        method: QuickMethod,
    },
    Gui(LaunchOptions),
}

pub const HELP: &str = "\
PCompare — compare files and folders

Usage:
  pcompare [options] <left> [<right>]                 compare two files or folders
  pcompare [options] <left> <right> <base> [<output>]  3-way merge (git mergetool order)
  pcompare @<script> [<param>…] [/silent]              run a script without a window
  pcompare /qc[=<method>] <left> <right>               quick compare without a window
  pcompare --snapshot <folder> <out.pcsnap> [--crc]    save a folder snapshot without a window

Options (Beyond Compare spellings in brackets):
  --viewer <kind>        [/fv=<viewer>] open as text, hex, table, picture, folder,
                         sync, merge, folderMerge, registry, version, mp3 or database
                         (BC names such as \"Text Compare\" and \"Hex Compare\" work too)
  --title-left <text>    [/title1=<text>] tab label for the left side
  --title-right <text>   [/title2=<text>] tab label for the right side
  --readonly             [/readonly, /ro] open both sides read-only
  --readonly-left        [/lro]  --readonly-right [/rro]
  --merge-output <path>  [/mergeoutput=<path>] where a merge writes its result
  --wait                 [/wait] don't return until the comparison's tab is closed
                         (implied by --merge-output); for git difftool / mergetool
  --base-first           paths are <base> <theirs> <yours> <merged> (P4MERGE order)
  -L <label>             label the next side (svn --diff-cmd passes these)
  --quick-compare[=<m>]  [/qc[=<m>]] compare headlessly, m = size, crc, binary or rules
                         (default rules: text rules for text files, bytes otherwise)
  --silent               [/silent] scripts: print only errors
  --select-left <path>   remember <path> for a later --compare-to
  --compare-to <path>    compare <path> with the remembered left side
  --compare-selection <path>
                         Explorer's 'Compare with PCompare': items started within
                         a moment of each other open as one comparison
  --install-shell-menu   add the Explorer right-click menu (unless turned off)
  --uninstall-shell-menu remove the Explorer right-click menu
  -h, --help             [/?, /h, /help] show this help

Exit codes:
  quick compare  0 = same (or only unimportant differences), 1 = different, 2 = error
  script         0 = success, 1 = some commands failed, 2 = unreadable or invalid script
  --wait         0 = closed (merges: the output was saved), 1 = merge output not saved
";

/// Session kinds the UI knows, by the names `--viewer` accepts.
fn viewer_kind(name: &str) -> Option<&'static str> {
    let key: String = name
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    let key = key
        .strip_suffix("compare")
        .filter(|k| !k.is_empty())
        .unwrap_or(&key);
    Some(match key {
        "text" => "text",
        "hex" | "binary" => "hex",
        "table" | "data" | "csv" => "table",
        "picture" | "image" => "picture",
        "folder" => "folder",
        "sync" | "foldersync" => "sync",
        "merge" | "textmerge" => "merge",
        "foldermerge" => "folderMerge",
        "registry" => "registry",
        "version" => "version",
        "mp3" | "audio" => "mp3",
        "database" | "db" => "database",
        _ => return None,
    })
}

/// A switch with its value: `/name=value`, `--name=value` or `--name value`.
struct Switch<'a> {
    name: String,
    value: Option<&'a str>,
}

/// Recognizes a switch. `/x` forms count only for known names, so absolute
/// Unix paths are never mistaken for switches.
fn switch(arg: &str) -> Option<Switch<'_>> {
    const SLASH: &[&str] = &[
        "fv",
        "title1",
        "title2",
        "readonly",
        "ro",
        "lro",
        "rro",
        "mergeoutput",
        "qc",
        "silent",
        "wait", // araxis:vcs
        "?",
        "h",
        "help",
    ];
    let (body, slash) = if let Some(b) = arg.strip_prefix("--") {
        (b, false)
    } else if let Some(b) = arg.strip_prefix('-') {
        (b, false)
    } else {
        (arg.strip_prefix('/')?, true)
    };
    let (name, value) = match body.split_once('=') {
        Some((n, v)) => (n, Some(v)),
        None => (body, None),
    };
    let name = name.to_ascii_lowercase();
    if name.is_empty() || (slash && !SLASH.contains(&name.as_str())) {
        return None;
    }
    Some(Switch { name, value })
}

/// Parses `args` (without the program name).
pub fn parse(args: &[String]) -> Result<Invocation, String> {
    let mut opts = LaunchOptions::default();
    let mut script: Option<String> = None;
    let mut silent = false;
    let mut quick: Option<QuickMethod> = None;
    let mut i = 0;
    // `--name value` takes the next argument when no `=value` was given.
    let take = |i: &mut usize, sw: &Switch, what: &str| -> Result<String, String> {
        if let Some(v) = sw.value {
            return Ok(v.to_owned());
        }
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("--{} needs {what}", sw.name))
    };
    while i < args.len() {
        let arg = &args[i];
        if let Some(file) = arg
            .strip_prefix('@')
            .filter(|f| !f.is_empty() && script.is_none() && opts.paths.is_empty())
        {
            script = Some(file.to_owned());
            i += 1;
            continue;
        }
        let Some(sw) = switch(arg) else {
            opts.paths.push(arg.clone());
            i += 1;
            continue;
        };
        match sw.name.as_str() {
            "h" | "help" | "?" => return Ok(Invocation::Help),
            "fv" | "viewer" => {
                let v = take(&mut i, &sw, "a viewer")?;
                let kind =
                    viewer_kind(&v).ok_or_else(|| format!("unknown viewer `{v}` (see --help)"))?;
                opts.viewer = Some(kind.to_owned());
            }
            "title1" | "title-left" | "lefttitle" => {
                opts.title_left = Some(take(&mut i, &sw, "a title")?)
            }
            "title2" | "title-right" | "righttitle" => {
                opts.title_right = Some(take(&mut i, &sw, "a title")?)
            }
            "readonly" | "ro" | "read-only" => {
                (opts.read_only_left, opts.read_only_right) = (true, true)
            }
            "lro" | "readonly-left" => opts.read_only_left = true,
            "rro" | "readonly-right" => opts.read_only_right = true,
            "mergeoutput" | "merge-output" | "output" => {
                opts.merge_output = Some(take(&mut i, &sw, "a path")?)
            }
            "qc" | "quick-compare" => {
                let m = sw.value.unwrap_or("rules");
                quick = Some(
                    QuickMethod::parse(m)
                        .ok_or_else(|| format!("unknown quick compare method `{m}`"))?,
                );
            }
            "silent" => silent = true,
            // araxis:vcs
            "wait" => opts.wait = true,
            "wait-token" => opts.wait_token = Some(take(&mut i, &sw, "a token")?),
            "base-first" => opts.base_first = true,
            // `-L <label>` (diff -L, svn --diff-cmd): left label, then right.
            "l" => {
                let label = take(&mut i, &sw, "a label")?;
                if opts.title_left.is_none() {
                    opts.title_left = Some(label);
                } else if opts.title_right.is_none() {
                    opts.title_right = Some(label);
                }
            }
            // araxis:vcs end
            "compare-to" => opts.compare_to = Some(take(&mut i, &sw, "a path")?),
            // Handled before parsing (records the path and exits).
            "select-left" => {
                take(&mut i, &sw, "a path")?;
            }
            // Unknown dash options (macOS adds `-psn_…`) are ignored, as before.
            _ => {}
        }
        i += 1;
    }
    if let Some(file) = script {
        return Ok(Invocation::Script {
            file,
            params: opts.paths,
            silent,
        });
    }
    if let Some(method) = quick {
        return match opts.paths.as_slice() {
            [l, r] => Ok(Invocation::QuickCompare {
                left: l.clone(),
                right: r.clone(),
                method,
            }),
            _ => Err("quick compare needs exactly two paths".into()),
        };
    }
    Ok(Invocation::Gui(opts))
}

/// The UI options for `args`, ignoring headless switches and errors (a bad
/// `--viewer` just opens the default viewer).
pub fn gui_options(args: &[String]) -> LaunchOptions {
    match parse(args) {
        Ok(Invocation::Gui(o)) => o,
        _ => LaunchOptions::default(),
    }
}

/// Applies `--compare-to`: pairs its path with the remembered left side (or
/// opens it alone when nothing was selected).
pub fn resolve(mut opts: LaunchOptions, left_selection: Option<String>) -> LaunchOptions {
    if let Some(target) = opts.compare_to.take() {
        opts.paths = match left_selection {
            Some(left) if left != target => vec![left, target],
            _ => vec![target],
        };
    }
    // araxis:vcs — `base theirs yours merged [extra…]` → `yours theirs base merged`.
    if opts.base_first && opts.paths.len() >= 3 {
        let mut p = std::mem::take(&mut opts.paths);
        p.truncate(4);
        p.swap(0, 2);
        opts.paths = p;
    }
    // araxis:vcs end
    if let Some(out) = opts.merge_output.clone() {
        // `/mergeoutput=` names the merge result: the 4th path.
        if opts.paths.len() >= 3 {
            opts.paths.truncate(3);
            opts.paths.push(out);
        }
    }
    opts
}

fn quick_compare(left: &str, right: &str, method: QuickMethod) -> i32 {
    match quick::quick_compare(Path::new(left), Path::new(right), method) {
        Ok(r) => {
            println!("{}", r.message);
            r.exit_code()
        }
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}

/// Runs headless invocations (help, scripts, quick compare) and returns
/// their exit code; `None` means "open the window".
pub fn run_headless(args: &[String]) -> Option<i32> {
    // bc5:archives — `pcompare --snapshot <folder> <out.pcsnap> [--crc]`
    if args.iter().any(|a| a.eq_ignore_ascii_case("--snapshot") || a.eq_ignore_ascii_case("/snapshot")) {
        pcompare_script::console::attach_parent();
        let code = crate::archive::run_snapshot_cli(args);
        let _ = std::io::stdout().flush();
        return code;
    }
    let invocation = match parse(args) {
        Ok(Invocation::Gui(_)) => return None,
        other => other,
    };
    pcompare_script::console::attach_parent();
    let code = match invocation {
        Ok(Invocation::Help) => {
            print!("{HELP}");
            0
        }
        Ok(Invocation::Script {
            file,
            params,
            silent,
        }) => pcompare_script::run_file(Path::new(&file), &params, silent),
        Ok(Invocation::QuickCompare {
            left,
            right,
            method,
        }) => quick_compare(&left, &right, method),
        Ok(Invocation::Gui(_)) => unreachable!("returned above"),
        Err(e) => {
            eprintln!("error: {e}\nRun `pcompare --help` for usage.");
            2
        }
    };
    let _ = std::io::stdout().flush();
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn gui(a: &[&str]) -> LaunchOptions {
        match parse(&args(a)).unwrap() {
            Invocation::Gui(o) => o,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plain_paths_pass_through_and_unknown_dashes_are_ignored() {
        assert_eq!(gui(&["a", "-psn_0_1", "b"]).paths, args(&["a", "b"]));
        // Absolute Unix paths are not switches.
        assert_eq!(
            gui(&["/Users/me/a", "/tmp/b"]).paths,
            args(&["/Users/me/a", "/tmp/b"])
        );
    }

    #[test]
    fn bc_and_gnu_style_switches() {
        let o = gui(&[
            "/fv=Hex Compare",
            "/title1=Mine",
            "/title2=Theirs",
            "/readonly",
            "a",
            "b",
        ]);
        assert_eq!(o.viewer.as_deref(), Some("hex"));
        assert_eq!(
            (o.title_left.as_deref(), o.title_right.as_deref()),
            (Some("Mine"), Some("Theirs"))
        );
        assert!(o.read_only_left && o.read_only_right);
        let o = gui(&[
            "--viewer",
            "table",
            "--title-left=L",
            "--readonly-right",
            "a",
            "b",
        ]);
        assert_eq!(o.viewer.as_deref(), Some("table"));
        assert_eq!(o.title_left.as_deref(), Some("L"));
        assert!(!o.read_only_left && o.read_only_right);
        assert_eq!(o.paths, args(&["a", "b"]));
        assert!(parse(&args(&["--viewer", "nope", "a"])).is_err());
        assert!(parse(&args(&["--title-left"])).is_err());
    }

    #[test]
    fn merge_output_becomes_the_fourth_path() {
        let o = resolve(gui(&["l", "r", "base", "/mergeoutput=out.txt"]), None);
        assert_eq!(o.paths, args(&["l", "r", "base", "out.txt"]));
        let o = resolve(
            gui(&["l", "r", "base", "old", "--merge-output", "new"]),
            None,
        );
        assert_eq!(o.paths, args(&["l", "r", "base", "new"]));
    }

    // araxis:vcs
    #[test]
    fn vcs_tool_switches() {
        let o = gui(&["/wait", "a", "b"]);
        assert!(o.wait && o.wait_token.is_none());
        assert_eq!(o.paths, args(&["a", "b"]));
        assert_eq!(gui(&["--wait-token=t1", "a"]).wait_token.as_deref(), Some("t1"));
        // P4MERGE: base theirs yours merged → yours theirs base merged.
        let o = resolve(gui(&["--base-first", "B", "T", "Y", "M", "wc"]), None);
        assert_eq!(o.paths, args(&["Y", "T", "B", "M"]));
        // svn --diff-cmd: `-u -L left -L right f1 f2`.
        let o = gui(&["-u", "-L", "a.c (rev 3)", "-L", "a.c (working copy)", "f1", "f2"]);
        assert_eq!(o.paths, args(&["f1", "f2"]));
        assert_eq!(o.title_left.as_deref(), Some("a.c (rev 3)"));
        assert_eq!(o.title_right.as_deref(), Some("a.c (working copy)"));
    }
    // araxis:vcs end

    #[test]
    fn compare_to_pairs_with_the_selected_left() {
        assert_eq!(
            resolve(gui(&["--compare-to", "b"]), Some("a".into())).paths,
            args(&["a", "b"])
        );
        assert_eq!(
            resolve(gui(&["--compare-to", "b"]), None).paths,
            args(&["b"])
        );
        assert_eq!(
            resolve(gui(&["--compare-to", "b"]), Some("b".into())).paths,
            args(&["b"])
        );
        assert!(parse(&args(&["--compare-to"])).is_err());
    }

    #[test]
    fn headless_invocations() {
        assert_eq!(parse(&args(&["/?"])).unwrap(), Invocation::Help);
        assert_eq!(parse(&args(&["-h"])).unwrap(), Invocation::Help);
        assert_eq!(
            parse(&args(&["/silent", "@s.txt", "p1", "p2"])).unwrap(),
            Invocation::Script {
                file: "s.txt".into(),
                params: args(&["p1", "p2"]),
                silent: true
            }
        );
        assert_eq!(
            parse(&args(&["/qc=binary", "a", "b"])).unwrap(),
            Invocation::QuickCompare {
                left: "a".into(),
                right: "b".into(),
                method: QuickMethod::Binary
            }
        );
        assert_eq!(
            parse(&args(&["--quick-compare", "a", "b"])).unwrap(),
            Invocation::QuickCompare {
                left: "a".into(),
                right: "b".into(),
                method: QuickMethod::Rules
            }
        );
        assert!(parse(&args(&["/qc", "a"])).is_err());
        assert!(parse(&args(&["/qc=fast", "a", "b"])).is_err());
    }

    #[test]
    fn viewer_names() {
        for (name, kind) in [
            ("Text Compare", "text"),
            ("text", "text"),
            ("Table Compare", "table"),
            ("Data Compare", "table"),
            ("Folder Merge", "folderMerge"),
            ("Text Merge", "merge"),
            ("MP3 Compare", "mp3"),
        ] {
            assert_eq!(viewer_kind(name), Some(kind), "{name}");
        }
        // Every kind maps onto one the app knows.
        for name in [
            "text",
            "hex",
            "table",
            "picture",
            "folder",
            "sync",
            "merge",
            "foldermerge",
            "registry",
            "version",
            "mp3",
            "database",
        ] {
            let kind = viewer_kind(name).unwrap();
            assert!(
                pcompare_core::session::SESSION_KINDS.contains(&kind),
                "{kind}"
            );
        }
    }
}
