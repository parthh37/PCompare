//! Remote write operations for the remote browser, and the bridge that
//! lets a server's sign-in questions reach the user (bc5:remote-write).
//!
//! Connections open on worker threads, often in the middle of a compare or
//! a sync. When an SSH server asks something no saved setting answers (a
//! one-time code, a password that wasn't saved), the [`UiPrompter`] emits a
//! `remote-auth-prompt` event and blocks that worker until the webview calls
//! [`remote_auth_answer`] — or gives up after a few minutes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pcompare_remote::prompt::{AuthPrompt, Prompter};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::{blocking, state, with_remote, CmdResult};

/// How long a connection waits for the user to answer.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PromptEvent<'a> {
    id: u64,
    prompt: &'a AuthPrompt,
}

type Answer = Option<Vec<String>>;

pub struct UiPrompter {
    app: AppHandle,
    next: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<Answer>>>,
}

impl Prompter for UiPrompter {
    fn ask(&self, prompt: &AuthPrompt) -> Answer {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let answer = match self.app.emit("remote-auth-prompt", PromptEvent { id, prompt }) {
            Ok(()) => rx.recv_timeout(ANSWER_TIMEOUT).ok().flatten(),
            Err(_) => None,
        };
        self.pending.lock().unwrap().remove(&id);
        answer
    }
}

struct Bridge(Arc<UiPrompter>);

/// Hooks the UI up as the remote manager's prompter. Called once at startup.
pub fn install_prompter(app: &AppHandle) {
    let prompter = Arc::new(UiPrompter {
        app: app.clone(),
        next: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
    });
    state(app).remote.set_prompter(prompter.clone());
    app.manage(Bridge(prompter));
}

/// The user's answers to prompt `id` (one per field), or `None` to cancel.
#[tauri::command]
pub fn remote_auth_answer(app: AppHandle, id: u64, answers: Option<Vec<String>>) {
    if let Some(bridge) = app.try_state::<Bridge>() {
        if let Some(tx) = bridge.0.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(answers);
        }
    }
}

/// Creates a folder on a remote.
#[tauri::command]
pub async fn remote_mkdir(app: AppHandle, path: String) -> CmdResult<()> {
    blocking(move || with_remote(&app, [&path, ""], |r| r.create_dir(&path))).await
}

/// Deletes a remote file or folder tree, permanently.
#[tauri::command]
pub async fn remote_delete(app: AppHandle, path: String) -> CmdResult<()> {
    blocking(move || with_remote(&app, [&path, ""], |r| r.delete(&path))).await
}

/// Renames a remote file or folder within its folder.
#[tauri::command]
pub async fn remote_rename(app: AppHandle, path: String, new_name: String) -> CmdResult<()> {
    blocking(move || with_remote(&app, [&path, ""], |r| r.rename(&path, &new_name))).await
}

/// Whether a remote can rename (the browser hides Rename otherwise).
#[tauri::command]
pub async fn remote_can_rename(app: AppHandle, path: String) -> CmdResult<bool> {
    blocking(move || {
        with_remote(&app, [&path, ""], |r| {
            Ok(r.target_write(&path)?.is_some_and(|(fs, _)| fs.caps().rename))
        })
    })
    .await
}
