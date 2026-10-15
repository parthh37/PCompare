//! Command-line interface, compatible with the common compare-tool switches
//! where that makes sense.
//!
//! ```text
//! pcompare [options] <left> [<right> [<base> [<output>]]]
//! pcompare @script.txt [params…] [/silent]
//! pcompare /qc[=size|crc|binary|rules] <left> <right>
//! pcompare <left> <right> <base> <output> /automerge [/force] [/reviewconflicts]
//! ```
//!
//! Switches may start with `/` (Windows style, matched only for the exact
//! names below so `/Users/…` stays a path) or `-`/`--`. Scripts, quick
//! compares, automatic merges and `--help` run without a window and exit
//! with the standard status codes; everything else becomes
//! [`LaunchOptions`] for the UI.

use pcompare_core::session::{SavedSession, SessionStore};
use pcompare_core::PathKind;
use pcompare_remote::profile::KeyringSecrets;
use pcompare_remote::provider::ProviderKind;
use pcompare_remote::RemoteManager;
use pcompare_script::host::{self, Host, Side};
use pcompare_script::merge::{self, AutoMerge, Favor};
use pcompare_script::quick::{self, QuickMethod};
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    /// Merge pane labels (`/title3=` center, `/title4=` output).
    pub title_base: Option<String>,
    pub title_output: Option<String>,
    /// `/filters=`: the name filter for the first folder comparison.
    pub filters: Option<String>,
    /// `/expandall`: expand every folder of the first folder comparison.
    pub expand_all: bool,
    /// `/savetarget=`: Save writes here instead of over the original.
    pub save_target: Option<String>,
    /// Sides that must not be edited (`/readonly`, `/lro`, `/rro`).
    pub read_only_left: bool,
    pub read_only_right: bool,
    /// Where a merge writes its result (`/mergeoutput=`).
    pub merge_output: Option<String>,
    /// `--compare-to <path>`: pair with the remembered left side.
    #[serde(skip)]
    pub compare_to: Option<String>,
    /// `--wait` / `/wait`: block until the tab closes (vcs.rs).
    #[serde(skip)]
    pub wait: bool,
    /// Set by the waiting process on the launch it forwards; the tab reports
    /// back under it when it closes.
    pub wait_token: Option<String>,
    /// `--base-first`: paths come as `base theirs yours merged` (P4MERGE).
    #[serde(skip)]
    pub base_first: bool,
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Help,
    Script {
        file: String,
        params: Vec<String>,
        silent: bool,
        /// `--dry-run`: list file operations instead of performing them.
        dry_run: bool,
    },
    QuickCompare {
        left: String,
        right: String,
        method: QuickMethod,
    },
    /// `/automerge`: merge without a window. With `review`
    /// (`/reviewconflicts`) a merge with conflicts opens in the app instead.
    AutoMerge {
        merge: AutoMerge,
        review: bool,
        gui: LaunchOptions,
    },
    Gui(LaunchOptions),
}

pub const HELP: &str = "\
PCompare — compare files and folders

Usage:
  pcompare [options] <left> [<right>]                 compare two files or folders
  pcompare [options] <left> <right> <base> [<output>]  3-way merge (git mergetool order)
  pcompare @<script> [<param>…] [/silent] [--dry-run] run a compare script without a window
  pcompare /qc[=<method>] <left> <right>               quick compare without a window
  pcompare <left> <right> <base> <output> /automerge   merge without a window (git mergetool)
  pcompare --snapshot <folder> <out.pcsnap> [--crc]    save a folder snapshot without a window

Options (alternative /switch spellings in brackets):
  --viewer <kind>        [/fv=<viewer>] open as text, hex, table, picture, folder,
                         sync, merge, folderMerge, registry, version, mp3 or database
                         (names such as \"Text Compare\" and \"Hex Compare\" work too)
  --title-left <text>    [/title1=<text>, /lefttitle=] label for the left side
  --title-right <text>   [/title2=<text>, /righttitle=] label for the right side
  --title-base <text>    [/title3=<text>, /centertitle=] label for the merge base
  --title-output <text>  [/title4=<text>, /outputtitle=] label for the merge output
  --readonly             [/readonly, /ro] open both sides read-only
  --readonly-left        [/lro]  --readonly-right [/rro]
  --merge-output <path>  [/mergeoutput=<path>] where a merge writes its result
  --center <path>        [/center=<path>] the merge base (instead of the 3rd path)
  --automerge            [/automerge] merge without a window; writes the output
                         unless there are conflicts
  --force                [/force] with /automerge: write conflicts with markers
  --review-conflicts     [/reviewconflicts] with /automerge: open merges that
                         have conflicts in the app (and wait for it)
  --ignore-unimportant   [/iu] with /automerge: whitespace changes don't conflict
  --favor-left           [/favorleft]  --favor-right [/favorright] take unimportant
                         changes from that side instead of the base
  --filters <masks>      [/filters=<masks>] name filter for a folder comparison
  --expand-all           [/expandall] expand every folder of a folder comparison
  --save-target <path>   [/savetarget=<path>] Save writes here instead
  --dry-run              scripts: list file operations instead of performing them
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

Exit codes (standard; changed in 1.5 — quick compare used to exit
0 for same and 1 for different, scripts 1 and 2 for errors):
  quick compare  1 = binary same, 2 = rules-based same (only encoding or line
                 endings differ), 11 = binary differences, 12 = similar (only
                 unimportant differences), 13 = rules-based differences, 100 = error
  script         0 = success, 100 = some commands failed, 105 = script unreadable,
                 106 = syntax error, 107 = a load failed (the script stops there)
  /automerge     0 = merged, 14 = conflicts written with markers (/force),
                 101 = conflicts, output not written, 100 = error
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
    // Every /switch, so none of them is taken for a path.
    const SLASH: &[&str] = &[
        "fv",
        "fileviewer",
        "title1",
        "title2",
        "title3",
        "title4",
        "lefttitle",
        "righttitle",
        "centertitle",
        "outputtitle",
        "readonly",
        "ro",
        "lro",
        "rro",
        "ro1",
        "ro2",
        "leftreadonly",
        "rightreadonly",
        "mergeoutput",
        "center",
        "automerge",
        "force",
        "reviewconflicts",
        "iu",
        "ignoreunimportant",
        "favorleft",
        "favorright",
        "qc",
        "quickcompare",
        "silent",
        "closescript",
        "solo",
        "nobackups",
        "expandall",
        "filters",
        "sync",
        "edit",
        "savetarget",
        "vcs1",
        "vcs2",
        "vcs3",
        "vcs4",
        "vcsleft",
        "vcsright",
        "vcscenter",
        "vcsoutput",
        "wait",
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
    let mut dry_run = false;
    let mut quick: Option<QuickMethod> = None;
    // `/automerge` and its companions.
    let (mut automerge, mut force, mut review, mut iu) = (false, false, false, false);
    let mut favor = Favor::None;
    let mut center: Option<String> = None;
    // `/vcs1=` … `/vcs4=`: shown where no title was given.
    let mut vcs: [Option<String>; 4] = Default::default();
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
            "title3" | "title-base" | "centertitle" => {
                opts.title_base = Some(take(&mut i, &sw, "a title")?)
            }
            "title4" | "title-output" | "outputtitle" => {
                opts.title_output = Some(take(&mut i, &sw, "a title")?)
            }
            "readonly" | "ro" | "read-only" => {
                (opts.read_only_left, opts.read_only_right) = (true, true)
            }
            "lro" | "ro1" | "leftreadonly" | "readonly-left" => opts.read_only_left = true,
            "rro" | "ro2" | "rightreadonly" | "readonly-right" => opts.read_only_right = true,
            "mergeoutput" | "merge-output" | "output" => {
                opts.merge_output = Some(take(&mut i, &sw, "a path")?)
            }
            "center" => center = Some(take(&mut i, &sw, "a path")?),
            "automerge" => automerge = true,
            "force" => force = true,
            "reviewconflicts" | "review-conflicts" => review = true,
            "iu" | "ignoreunimportant" | "ignore-unimportant" => iu = true,
            "favorleft" | "favor-left" => favor = Favor::Left,
            "favorright" | "favor-right" => favor = Favor::Right,
            "filters" => opts.filters = Some(take(&mut i, &sw, "file masks")?),
            "expandall" | "expand-all" => opts.expand_all = true,
            "savetarget" | "save-target" => opts.save_target = Some(take(&mut i, &sw, "a path")?),
            // `/sync` and `/edit` open those viewers.
            "sync" => opts.viewer = Some("sync".into()),
            "edit" => opts.viewer = Some("edit".into()),
            "vcs1" | "vcsleft" | "vcs2" | "vcsright" | "vcs3" | "vcscenter" | "vcs4"
            | "vcsoutput" => {
                let n = match sw.name.as_str() {
                    "vcs1" | "vcsleft" => 0,
                    "vcs2" | "vcsright" => 1,
                    "vcs3" | "vcscenter" => 2,
                    _ => 3,
                };
                vcs[n] = Some(take(&mut i, &sw, "a path")?);
            }
            "dry-run" | "dryrun" => dry_run = true,
            // Window and backup behaviour PCompare doesn't have.
            "closescript" | "solo" | "nobackups" => {}
            "qc" | "quick-compare" | "quickcompare" => {
                let m = sw.value.unwrap_or("rules");
                quick = Some(
                    QuickMethod::parse(m)
                        .ok_or_else(|| format!("unknown quick compare method `{m}`"))?,
                );
            }
            "silent" => silent = true,
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
            "compare-to" => opts.compare_to = Some(take(&mut i, &sw, "a path")?),
            // Handled before parsing (records the path and exits).
            "select-left" => {
                take(&mut i, &sw, "a path")?;
            }
            // automation:shell — start-at-login launches hidden in the tray (main.rs).
            "background" => {}
            // automation:shell end
            // Unknown dash options (macOS adds `-psn_…`) are ignored, as before.
            _ => {}
        }
        i += 1;
    }
    for (title, path) in [
        (&mut opts.title_left, &vcs[0]),
        (&mut opts.title_right, &vcs[1]),
        (&mut opts.title_base, &vcs[2]),
        (&mut opts.title_output, &vcs[3]),
    ] {
        if title.is_none() {
            title.clone_from(path);
        }
    }
    if let Some(file) = script {
        return Ok(Invocation::Script {
            file,
            params: opts.paths,
            silent,
            dry_run,
        });
    }
    if let Some(c) = &center {
        // `/center=` names the base: the third path.
        if opts.paths.len() >= 3 {
            opts.paths[2] = c.clone();
        } else if opts.paths.len() == 2 {
            opts.paths.push(c.clone());
        }
    }
    if automerge {
        // A merge reviewed in the app must keep the version control system waiting.
        opts.wait = true;
        // The relaunched window (`--wait-token`) just shows the merge.
        if opts.wait_token.is_none() {
            let paths = resolve(opts.clone(), None).paths;
            let output = paths
                .get(3)
                .cloned()
                .ok_or("/automerge needs an output file: a 4th path or /mergeoutput=")?;
            let (left, right) = match paths.as_slice() {
                [l, r, ..] => (l.clone(), r.clone()),
                _ => return Err("/automerge needs a left, a right and an output file".into()),
            };
            return Ok(Invocation::AutoMerge {
                merge: AutoMerge {
                    left,
                    right,
                    base: paths.get(2).cloned().filter(|b| !b.is_empty()),
                    output,
                    force,
                    ignore_unimportant: iu,
                    favor,
                },
                review,
                gui: opts,
            });
        }
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
        // `/automerge /reviewconflicts` with conflicts opens the merge.
        Ok(Invocation::AutoMerge { gui, .. }) => gui,
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
    // `base theirs yours merged [extra…]` → `yours theirs base merged`.
    if opts.base_first && opts.paths.len() >= 3 {
        let mut p = std::mem::take(&mut opts.paths);
        p.truncate(4);
        p.swap(0, 2);
        opts.paths = p;
    }
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
            quick::EXIT_ERROR
        }
    }
}

/// `/automerge`: `None` when conflicts should be reviewed in the window.
fn auto_merge(m: &AutoMerge, review: bool) -> Option<i32> {
    if review {
        // Merge without writing first: only a clean merge is written headless.
        let probe = AutoMerge {
            force: false,
            ..m.clone()
        };
        return match merge::auto_merge(&probe) {
            Ok(r) if r.conflicts > 0 => None,
            Ok(r) => {
                println!("{}", r.message(&m.output));
                Some(r.exit_code())
            }
            Err(e) => {
                eprintln!("error: {e}");
                Some(merge::EXIT_ERROR)
            }
        };
    }
    Some(match merge::auto_merge(m) {
        Ok(r) => {
            println!("{}", r.message(&m.output));
            r.exit_code()
        }
        Err(e) => {
            eprintln!("error: {e}");
            merge::EXIT_ERROR
        }
    })
}

/// Runs headless invocations (help, scripts, quick compare) and returns
/// their exit code; `None` means "open the window".
pub fn run_headless(args: &[String]) -> Option<i32> {
    // `pcompare --snapshot <folder> <out.pcsnap> [--crc]`
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
            dry_run,
        }) => {
            let host = AppHost::new();
            pcompare_script::run_file(
                Path::new(&file),
                &pcompare_script::RunOptions {
                    params: &params,
                    silent,
                    dry_run,
                    host: &host,
                },
            )
        }
        Ok(Invocation::QuickCompare {
            left,
            right,
            method,
        }) => quick_compare(&left, &right, method),
        Ok(Invocation::AutoMerge { merge, review, .. }) => auto_merge(&merge, review)?,
        Ok(Invocation::Gui(_)) => unreachable!("returned above"),
        Err(e) => {
            eprintln!("error: {e}\nRun `pcompare --help` for usage.");
            merge::EXIT_ERROR
        }
    };
    let _ = std::io::stdout().flush();
    Some(code)
}

// ---------- the script host ----------

/// The app's identifier (tauri.conf.json), which names its settings folder.
const APP_IDENTIFIER: &str = "com.parthh37.pcompare";

/// The app's settings folder without a running app (Tauri's
/// `app_config_dir`): sessions, connection profiles and file formats.
fn config_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        env("APPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support"))
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))
    };
    base.map(|b| b.join(APP_IDENTIFIER))
}

/// Scripts' view of the app: base folders resolve exactly as in Folder
/// Compare ([`crate::archive::folder_side`]: local and remote folders,
/// archives, snapshots, `svn:` / `git:`), with the user's saved sessions,
/// connection profiles and file format rules. Script-style
/// `ftp://user@host/path` and `sftp://…` URLs use the matching saved
/// FTP / SFTP connection.
pub struct AppHost {
    remote: RemoteManager,
    sessions: Option<SessionStore>,
    formats: pcompare_convert::ConvertConfig,
    scratch: PathBuf,
}

impl AppHost {
    pub fn new() -> Self {
        Self::with_config(config_dir(), Box::new(KeyringSecrets))
    }

    /// A host reading sessions, connections and file formats from `config`,
    /// with connection secrets in `secrets`.
    pub fn with_config(
        config: Option<PathBuf>,
        secrets: Box<dyn pcompare_core::secrets::SecretStore>,
    ) -> Self {
        let scratch = std::env::temp_dir().join(format!("pcompare-script-{}", std::process::id()));
        let remote = RemoteManager::new(
            config.as_deref().unwrap_or(&scratch),
            scratch.join("remote"),
            secrets,
        );
        let formats = config
            .as_ref()
            .and_then(|c| pcompare_convert::ConvertConfig::load(&c.join(crate::convert::CONFIG_FILE)).ok())
            .unwrap_or_default();
        AppHost {
            remote,
            sessions: config.map(|c| SessionStore::new(c.join("sessions.json"))),
            formats,
            scratch,
        }
    }

    /// `ftp://user@host/dir` → `remote://<profile>/dir` through a saved
    /// connection with that host (and user); other paths as they are.
    fn map_url(&self, path: &str) -> Result<String, String> {
        let Some((scheme, rest)) = path.split_once("://") else {
            return Ok(path.to_owned());
        };
        let kinds: &[ProviderKind] = match scheme.to_ascii_lowercase().as_str() {
            "ftp" | "ftps" => &[ProviderKind::Ftp],
            "sftp" | "ssh" => &[ProviderKind::Sftp],
            _ => return Ok(path.to_owned()),
        };
        let (authority, dir) = rest.split_once('/').unwrap_or((rest, ""));
        let (user, host) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u.split(':').next().unwrap_or(u)), h),
            None => (None, authority),
        };
        let host = host.split(':').next().unwrap_or(host);
        let profiles = self.remote.profiles.list().map_err(|e| e.to_string())?;
        let found = profiles.iter().find(|p| {
            let get = |k: &str| p.settings.get(k).map(String::as_str).unwrap_or("");
            kinds.contains(&p.kind)
                && get("host").eq_ignore_ascii_case(host)
                && user.is_none_or(|u| get("user") == u)
        });
        let Some(p) = found else {
            return Err(format!(
                "{path}: no saved connection for {host}; add one under Connections first"
            ));
        };
        let root = p.settings.get("root").map(|r| r.trim_matches('/')).unwrap_or("");
        let dir = dir.trim_matches('/');
        let rel = if root.is_empty() {
            dir
        } else if dir == root {
            ""
        } else {
            dir.strip_prefix(root)
                .and_then(|d| d.strip_prefix('/'))
                .ok_or_else(|| format!("{path} is outside the connection's root folder /{root}"))?
        };
        Ok(format!("remote://{}/{rel}", p.id))
    }
}

impl Drop for AppHost {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

impl Host for AppHost {
    fn kind(&self, path: &str) -> PathKind {
        if path.contains("://") {
            return PathKind::Directory;
        }
        host::local_kind(path)
    }

    fn folder(&self, path: &str) -> Result<Side, String> {
        let path = self.map_url(path)?;
        let local = Path::new(&path);
        // Snapshots saved under the legacy `.bcss` name, and plain folders.
        if local.is_dir() || (local.is_file() && host::is_snapshot_name(&path)) {
            return host::container_or_local(self.remote.archives(), &path);
        }
        let side = crate::archive::folder_side(&self.remote, &path, false).map_err(|e| {
            self.remote.forget(&path);
            e.to_string()
        })?;
        let write = self.remote.target_write_folder(&path).map_err(|e| e.to_string())?;
        Ok(Side {
            shown: path.clone(),
            fs: side.fs,
            path: side.path,
            local: None,
            write,
            crcs: side.crcs,
            snapshot: side.snapshot,
        })
    }

    fn create_folder(&self, path: &str) -> Result<(), String> {
        let path = self.map_url(path)?;
        if pcompare_remote::is_remote(&path) {
            return self.remote.create_dir(&path).map_err(|e| e.to_string());
        }
        std::fs::create_dir_all(&path).map_err(|e| format!("{path}: {e}"))
    }

    fn local_file(&self, path: &str) -> Result<PathBuf, String> {
        let path = self.map_url(path)?;
        self.remote.local_copy(&path, true).map_err(|e| e.to_string())
    }

    fn session(&self, name: &str) -> Option<SavedSession> {
        host::find_session(self.sessions.as_ref()?, name)
    }

    fn file_formats(&self) -> pcompare_convert::ConvertConfig {
        self.formats.clone()
    }

    fn archives(&self) -> &Arc<pcompare_archive::Archives> {
        self.remote.archives()
    }
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

    // automation:shell
    #[test]
    fn background_is_a_switch_not_a_path() {
        assert!(gui(&["--background"]).paths.is_empty());
        assert_eq!(gui(&["--background", "a"]).paths, args(&["a"]));
    }
    // automation:shell end

    #[test]
    fn slash_and_gnu_style_switches() {
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
                silent: true,
                dry_run: false,
            }
        );
        assert!(matches!(
            parse(&args(&["@s.txt", "--dry-run"])).unwrap(),
            Invocation::Script { dry_run: true, .. }
        ));
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

    /// A scratch folder removed on drop.
    struct Temp(PathBuf);

    impl Temp {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir() -> Temp {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pcompare-cli-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Temp(dir)
    }

    #[test]
    fn automerge_switches() {
        let m = |a: &[&str]| match parse(&args(a)).unwrap() {
            Invocation::AutoMerge { merge, review, .. } => (merge, review),
            other => panic!("{other:?}"),
        };
        let (a, review) = m(&["L", "R", "B", "O", "/automerge", "/force", "/iu", "/favorright"]);
        assert_eq!(
            a,
            AutoMerge {
                left: "L".into(),
                right: "R".into(),
                base: Some("B".into()),
                output: "O".into(),
                force: true,
                ignore_unimportant: true,
                favor: Favor::Right,
            }
        );
        assert!(!review);
        // `/center=` and `/mergeoutput=` name the base and the output.
        let (a, review) = m(&["/automerge", "/reviewconflicts", "/center=C", "L", "R", "/mergeoutput=O"]);
        assert_eq!((a.base.as_deref(), a.output.as_str(), review), (Some("C"), "O", true));
        assert_eq!(m(&["-automerge", "-favorleft", "L", "R", "B", "O"]).0.favor, Favor::Left);
        assert!(parse(&args(&["L", "R", "B", "/automerge"])).is_err());
        // The window a review opens (relaunched with a token) just shows the merge.
        let o = gui(&["L", "R", "B", "O", "/automerge", "/reviewconflicts", "--wait-token=t"]);
        assert_eq!(o.paths, args(&["L", "R", "B", "O"]));
        // …and the first launch waits for it.
        assert!(gui_options(&args(&["L", "R", "B", "O", "/automerge", "/reviewconflicts"])).wait);
    }

    #[test]
    fn automerge_runs_headless_with_standard_exit_codes() {
        let d = temp_dir();
        let p = |n: &str| d.path().join(n).to_string_lossy().into_owned();
        // Edits two unchanged lines apart: past the default conflict scan lines.
        std::fs::write(p("b"), "a\nb\nc\nd\n").unwrap();
        std::fs::write(p("l"), "A\nb\nc\nd\n").unwrap();
        std::fs::write(p("r"), "a\nb\nc\nD\n").unwrap();
        let run = |extra: &[&str]| {
            let mut a = args(&[&p("l"), &p("r"), &p("b"), &p("o")]);
            a.extend(args(extra));
            run_headless(&a)
        };
        assert_eq!(run(&["/automerge"]), Some(0));
        assert_eq!(std::fs::read_to_string(p("o")).unwrap(), "A\nb\nc\nD\n");
        std::fs::write(p("r"), "Z\nb\nc\nd\n").unwrap();
        std::fs::remove_file(p("o")).unwrap();
        assert_eq!(run(&["/automerge"]), Some(101));
        assert!(!d.path().join("o").exists());
        // Conflicts to review open the window.
        assert_eq!(run(&["/automerge", "/reviewconflicts"]), None);
        assert_eq!(run(&["/automerge", "/force"]), Some(14));
        assert!(std::fs::read_to_string(p("o")).unwrap().contains("<<<<<<< Left #1"));
    }

    #[test]
    fn quick_compare_exit_codes() {
        let d = temp_dir();
        let p = |n: &str| d.path().join(n).to_string_lossy().into_owned();
        std::fs::write(p("a"), "x\r\n").unwrap();
        std::fs::write(p("b"), "x\n").unwrap();
        let qc = |m: &str, l: &str, r: &str| run_headless(&args(&[m, &p(l), &p(r)]));
        assert_eq!(qc("/qc=binary", "a", "a"), Some(1));
        assert_eq!(qc("/qc", "a", "b"), Some(2));
        assert_eq!(qc("/qc=binary", "a", "b"), Some(11));
        assert_eq!(qc("/qc", "a", "nope"), Some(100));
    }

    #[test]
    fn titles_vcs_paths_and_other_slash_switches() {
        let o = gui(&[
            "/title3=Base",
            "/outputtitle=Merged",
            "/vcs1=a.c#12",
            "/vcs2=a.c#13",
            "/title2=Mine",
            "/solo",
            "/nobackups",
            "/closescript",
            "/expandall",
            "/filters=*.c;-obj\\",
            "/savetarget=out.c",
            "/ro1",
            "l",
            "r",
        ]);
        assert_eq!(o.paths, args(&["l", "r"]));
        assert_eq!(o.title_base.as_deref(), Some("Base"));
        assert_eq!(o.title_output.as_deref(), Some("Merged"));
        // A /vcs path stands in for a missing title only.
        assert_eq!(o.title_left.as_deref(), Some("a.c#12"));
        assert_eq!(o.title_right.as_deref(), Some("Mine"));
        assert!(o.expand_all && o.read_only_left && !o.read_only_right);
        assert_eq!(o.filters.as_deref(), Some("*.c;-obj\\"));
        assert_eq!(o.save_target.as_deref(), Some("out.c"));
        assert_eq!(gui(&["/sync", "a", "b"]).viewer.as_deref(), Some("sync"));
        assert_eq!(gui(&["/edit", "a"]).viewer.as_deref(), Some("edit"));
    }

    #[test]
    fn app_identifier_matches_tauri_conf() {
        let conf = include_str!("../tauri.conf.json");
        assert!(conf.contains(&format!("\"identifier\": \"{APP_IDENTIFIER}\"")));
    }

    #[test]
    fn script_host_maps_ftp_urls_to_saved_connections() {
        let d = temp_dir();
        // Never the real keychain in tests: profiles are sealed with a key kept there.
        let host = AppHost::with_config(
            Some(d.path().to_path_buf()),
            Box::new(pcompare_core::secrets::MemorySecrets::default()),
        );
        let mut settings = std::collections::BTreeMap::new();
        settings.insert("host".to_owned(), "website.com".to_owned());
        settings.insert("user".to_owned(), "user".to_owned());
        settings.insert("root".to_owned(), "/www".to_owned());
        let profile = host
            .remote
            .profiles
            .save(pcompare_remote::profile::ProfileInput {
                id: None,
                name: "Web".into(),
                kind: ProviderKind::Ftp,
                settings,
                secrets: Default::default(),
            })
            .unwrap();
        assert_eq!(
            host.map_url("ftp://user@website.com/www/folder").unwrap(),
            format!("remote://{}/folder", profile.id)
        );
        assert!(host.map_url("ftp://other@website.com/www").is_err());
        assert!(host.map_url("ftp://website.com/elsewhere").is_err());
        assert_eq!(host.map_url("/local/path").unwrap(), "/local/path");
        // Local folders and `.bcss`-named snapshots go through the same sides as the app.
        std::fs::create_dir_all(d.path().join("l")).unwrap();
        let side = host.folder(&d.path().join("l").to_string_lossy()).unwrap();
        assert!(side.local.is_some());
        assert!(host.session("nope").is_none());
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
