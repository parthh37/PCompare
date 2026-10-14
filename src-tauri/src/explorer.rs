//! Explorer right-click menu: the windowless launches its verbs make, the
//! startup check that (re)registers the menu for this copy, and the Settings
//! commands. The registry and spool code lives in `pcompare-shell`.

use crate::{left_selection_file, read_left_selection, CmdResult};
use pcompare_shell::menu::{self, MenuStatus};
use pcompare_shell::selection::{self, Spool};
use std::time::Duration;

/// How long the selection collector waits for more items from the same
/// right-click, and its upper bound when Explorer is slow to start them.
const SELECTION_QUIET: Duration = Duration::from_millis(350);
const SELECTION_MAX: Duration = Duration::from_secs(3);

fn current_exe() -> Option<String> {
    std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned())
}

/// The value after `flag`, if `flag` is present.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<Option<&'a String>> {
    args.iter().position(|a| a == flag).map(|i| args.get(i + 1))
}

/// Handles the launches that never open a window. `Some(exit code)` when
/// `args` (without the program name) was one of them.
pub fn run_early(args: &[String]) -> Option<i32> {
    // "Select Left Side for Compare" records the path and exits, like Beyond
    // Compare's shell extension; the menu's "Compare to" then names it.
    if let Some(path) = flag_value(args, "--select-left") {
        if let Some(path) = path {
            let _ = std::fs::write(left_selection_file(), path);
            menu::set_left(Some(path));
        }
        return Some(0);
    }
    if let Some(path) = flag_value(args, "--compare-selection") {
        return Some(path.map_or(2, |p| compare_selection(p)));
    }
    // Installer hooks. Install respects "off" in Settings, which uninstall keeps
    // (like the app's other settings) so a reinstall doesn't bring the menu back.
    if args.iter().any(|a| a == "--install-shell-menu") {
        let ok = current_exe().is_some_and(|exe| menu::ensure(&exe, read_left_selection().as_deref()).is_ok());
        return Some(if ok { 0 } else { 1 });
    }
    if args.iter().any(|a| a == "--uninstall-shell-menu") {
        return Some(if menu::unregister().is_ok() { 0 } else { 1 });
    }
    None
}

/// One item of a "Compare with PCompare" multi-selection. The collecting
/// process relaunches PCompare with every item as ordinary paths, so the
/// single-instance handoff and viewer detection work exactly as for
/// `pcompare a b`.
fn compare_selection(path: &str) -> i32 {
    let spool = Spool::default_location();
    if spool.add(path).is_err() {
        return relaunch(&["--compare-to".to_owned(), path.to_owned()]);
    }
    let claim = match spool.try_claim() {
        Ok(Some(claim)) => claim,
        // Another process is collecting and will pick this item up.
        Ok(None) => return 0,
        Err(_) => return relaunch(&["--compare-to".to_owned(), path.to_owned()]),
    };
    let paths = selection::arrange(claim.collect(SELECTION_QUIET, SELECTION_MAX));
    match paths.len() {
        0 => 0,
        // A single item pairs with "Select Left Side", or opens on its own.
        1 => relaunch(&["--compare-to".to_owned(), paths[0].clone()]),
        _ => relaunch(&paths),
    }
}

fn relaunch(args: &[String]) -> i32 {
    let Some(exe) = current_exe() else { return 1 };
    match std::process::Command::new(exe).args(args).spawn() {
        Ok(_) => 0,
        Err(_) => 1,
    }
}

/// Startup: put the menu back if it's missing or runs another copy (an MSI
/// install never runs the NSIS hook; a moved install leaves stale verbs).
/// Debug builds leave it alone so `tauri dev` doesn't point Explorer at them.
pub fn ensure_on_startup() {
    if cfg!(debug_assertions) {
        return;
    }
    std::thread::spawn(|| {
        if let Some(exe) = current_exe() {
            let _ = menu::ensure(&exe, read_left_selection().as_deref());
        }
    });
}

#[tauri::command]
pub fn shell_menu_status() -> MenuStatus {
    current_exe().map_or(MenuStatus::Unsupported, |exe| menu::status(&exe))
}

/// Settings: turn the menu on (registering this copy) or off (removing it).
#[tauri::command]
pub fn shell_menu_set(enabled: bool) -> CmdResult<MenuStatus> {
    let exe = current_exe().ok_or("can't locate PCompare's executable")?;
    let done = if enabled {
        menu::set_disabled(false).and_then(|_| menu::register(&exe, read_left_selection().as_deref()))
    } else {
        menu::set_disabled(true)
    };
    done.map_err(|e| format!("couldn't update the Explorer menu: {e}"))?;
    Ok(menu::status(&exe))
}
