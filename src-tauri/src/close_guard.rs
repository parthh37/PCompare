//! Unsaved-edits guard for closing the main window and quitting the app.
//!
//! The UI knows which tabs hold unsaved edits; it reports "any unsaved" here
//! so the window-close handler, the Quit menu items and `ExitRequested` can
//! decide synchronously. When something would be lost, the close or exit is
//! stopped and the UI is asked (`close-guard-ask`) to offer Save all /
//! Discard / Cancel; it answers with `close_guard_proceed`.

use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, RunEvent, RESTART_EXIT_CODE};

/// Emitted to the main window with [`AskPayload`] when a close or quit
/// was stopped because of unsaved edits.
pub const ASK_EVENT: &str = "close-guard-ask";
/// Id of the macOS app menu's Quit item, which replaces the predefined one
/// (that calls `terminate:` directly, which can't be stopped).
pub const QUIT_MENU_ID: &str = "app-quit";

const MAIN_WINDOW: &str = "main";

#[derive(Default)]
pub struct CloseGuard {
    /// Some tab holds unsaved edits (reported by the UI).
    unsaved: AtomicBool,
    /// The user chose Save all or Discard: the next close or exit goes ahead.
    released: AtomicBool,
}

impl CloseGuard {
    fn blocks(&self) -> bool {
        self.unsaved.load(Ordering::SeqCst) && !self.released.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct AskPayload {
    /// The app is quitting (rather than just the window closing).
    quit: bool,
}

/// What closing the main window does.
#[derive(Debug, PartialEq, Eq)]
pub enum CloseAction {
    Close,
    /// Close-to-tray: the window is hidden, so its edits survive.
    HideToTray,
    /// Stop the close and ask about unsaved edits.
    Ask,
}

pub fn close_action(hide_to_tray: bool, blocks: bool) -> CloseAction {
    if hide_to_tray {
        CloseAction::HideToTray
    } else if blocks {
        CloseAction::Ask
    } else {
        CloseAction::Close
    }
}

/// Whether an exit request must be stopped and asked about. A restart
/// (the updater) can't be stopped, so it isn't asked about either.
pub fn exit_needs_asking(blocks: bool, code: Option<i32>) -> bool {
    blocks && code != Some(RESTART_EXIT_CODE)
}

pub fn manage(app: &AppHandle) {
    app.manage(CloseGuard::default());
}

/// Whether closing or quitting now would lose unsaved edits.
pub fn blocks(app: &AppHandle) -> bool {
    app.try_state::<CloseGuard>().is_some_and(|g| g.blocks())
}

/// Shows the main window and asks the UI about unsaved edits.
pub fn ask(app: &AppHandle, quit: bool) {
    crate::automation::show_main_window(app);
    let _ = app.emit_to(MAIN_WINDOW, ASK_EVENT, AskPayload { quit });
}

/// A quit that was asked about was then called off (e.g. "Keep running"
/// while a sync runs): unsaved edits are guarded again.
pub fn rearm(app: &AppHandle) {
    if let Some(g) = app.try_state::<CloseGuard>() {
        g.released.store(false, Ordering::SeqCst);
    }
}

/// Quit requests from menus: ask first when edits would be lost.
pub fn request_quit(app: &AppHandle) {
    if blocks(app) {
        ask(app, true);
    } else {
        crate::automation::quit(app.clone());
    }
}

/// Stops exits that would lose unsaved edits (e.g. the last window closed
/// some other way, or `exit` called without going through the guard).
pub fn on_run_event(app: &AppHandle, event: &RunEvent) {
    if let RunEvent::ExitRequested { code, api, .. } = event {
        if exit_needs_asking(blocks(app), *code) {
            api.prevent_exit();
            ask(app, true);
        }
    }
}

/// On macOS, swaps the default menu's Quit (which terminates at once) for
/// one that goes through [`request_quit`].
#[cfg(target_os = "macos")]
pub fn install_app_menu(app: &AppHandle) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    let menu = Menu::default(app)?;
    if let Some(app_menu) = menu.items()?.first().and_then(|i| i.as_submenu().cloned()) {
        let last = app_menu.items()?.len().saturating_sub(1);
        app_menu.remove_at(last)?;
        let name = &app.package_info().name;
        app_menu.append(&MenuItem::with_id(
            app,
            QUIT_MENU_ID,
            format!("Quit {name}"),
            true,
            Some("CmdOrCtrl+Q"),
        )?)?;
    }
    app.set_menu(menu)?;
    app.on_menu_event(|app, event| {
        if event.id().as_ref() == QUIT_MENU_ID {
            request_quit(app);
        }
    });
    Ok(())
}

#[tauri::command]
pub fn close_guard_set_unsaved(guard: tauri::State<'_, CloseGuard>, unsaved: bool) {
    guard.unsaved.store(unsaved, Ordering::SeqCst);
    if !unsaved {
        guard.released.store(false, Ordering::SeqCst);
    }
}

/// The user saved everything or chose Discard: close the window, or quit.
#[tauri::command]
pub fn close_guard_proceed(app: AppHandle, guard: tauri::State<'_, CloseGuard>, quit: bool) {
    guard.released.store(true, Ordering::SeqCst);
    if quit {
        crate::automation::quit(app);
    } else if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_hides_to_tray_before_asking() {
        // Hiding keeps the tabs (and their edits) alive, so nothing to ask.
        assert_eq!(close_action(true, true), CloseAction::HideToTray);
        assert_eq!(close_action(true, false), CloseAction::HideToTray);
        assert_eq!(close_action(false, true), CloseAction::Ask);
        assert_eq!(close_action(false, false), CloseAction::Close);
    }

    #[test]
    fn exits_are_asked_about_unless_restarting() {
        assert!(exit_needs_asking(true, None));
        assert!(exit_needs_asking(true, Some(0)));
        assert!(!exit_needs_asking(true, Some(RESTART_EXIT_CODE)));
        assert!(!exit_needs_asking(false, None));
    }

    #[test]
    fn released_guard_lets_one_close_through_until_rearmed() {
        let g = CloseGuard::default();
        assert!(!g.blocks());
        g.unsaved.store(true, Ordering::SeqCst);
        assert!(g.blocks());
        g.released.store(true, Ordering::SeqCst);
        assert!(!g.blocks());
        g.released.store(false, Ordering::SeqCst);
        assert!(g.blocks());
    }
}
