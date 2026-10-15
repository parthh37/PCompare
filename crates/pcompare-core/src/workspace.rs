//! Workspaces, the automatic "recent sessions" history and app preferences.
//!
//! A workspace is a named snapshot of every open tab (what each compares, its
//! view settings and which saved session it belongs to) plus the active tab. The store also keeps one unnamed "last"
//! workspace — the tabs that were open when the app last ran — so the next
//! start can restore them.
//!
//! Recent sessions are "auto-saved sessions": compares that were closed
//! without ever being saved, newest first and capped at [`RECENT_LIMIT`].
//!
//! Like saved sessions, tab settings and preferences are opaque JSON owned by
//! the UI; the engine only validates structure.

use crate::session::{resolve_name_conflict, SESSION_KINDS};
use crate::store::{new_id, now_ms, read_json, write_json};
use crate::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// How many closed, never-saved compares the recent list remembers.
pub const RECENT_LIMIT: usize = 20;

/// Tab kinds that aren't comparisons but can still be part of a workspace.
const SHELL_KINDS: &[&str] = &["home", "profiles"];

fn known_tab_kind(kind: &str) -> bool {
    SESSION_KINDS.contains(&kind) || SHELL_KINDS.contains(&kind)
}

/// One open tab inside a workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTab {
    pub kind: String,
    #[serde(default)]
    pub left: String,
    #[serde(default)]
    pub right: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default)]
    pub settings: serde_json::Value,
    /// The saved session this tab was opened from or saved as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub tabs: Vec<WorkspaceTab>,
    /// Index into `tabs` of the tab that was in front.
    #[serde(default)]
    pub active: usize,
    pub created_ms: i64,
    pub modified_ms: i64,
}

/// What the UI sends to save a workspace: `id` set = overwrite that one.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInput {
    pub id: Option<String>,
    pub name: String,
    pub tabs: Vec<WorkspaceTab>,
    #[serde(default)]
    pub active: usize,
}

#[derive(Default, Serialize, Deserialize)]
struct WorkspaceFile {
    #[serde(default)]
    workspaces: Vec<Workspace>,
    /// The tabs open when the app last closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last: Option<Workspace>,
}

/// A JSON file guarded against concurrent read-modify-write cycles from IPC.
struct JsonFile {
    path: PathBuf,
    lock: Mutex<()>,
}

impl JsonFile {
    fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Keeps `active` pointing at a real tab (or 0 for an empty list).
fn clamp_active(active: usize, tabs: &[WorkspaceTab]) -> usize {
    active.min(tabs.len().saturating_sub(1))
}

pub struct WorkspaceStore {
    file: JsonFile,
}

impl WorkspaceStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            file: JsonFile::new(path),
        }
    }

    /// Named workspaces, most recently saved first.
    pub fn list(&self) -> Result<Vec<Workspace>> {
        let _g = self.file.guard();
        let mut file: WorkspaceFile = read_json(&self.file.path)?;
        file.workspaces.sort_by_key(|w| std::cmp::Reverse(w.modified_ms));
        Ok(file.workspaces)
    }

    pub fn save(&self, input: WorkspaceInput) -> Result<Workspace> {
        let name = input.name.trim();
        if name.is_empty() {
            return Err(CoreError::Other("workspace name cannot be empty".into()));
        }
        if input.tabs.is_empty() {
            return Err(CoreError::Other("a workspace needs at least one tab".into()));
        }
        if let Some(bad) = input.tabs.iter().find(|t| !known_tab_kind(&t.kind)) {
            return Err(CoreError::Other(format!("unknown session kind `{}`", bad.kind)));
        }
        let _g = self.file.guard();
        let mut file: WorkspaceFile = read_json(&self.file.path)?;
        let id = input.id.as_deref();
        let taken: Vec<&str> = file
            .workspaces
            .iter()
            .filter(|w| Some(w.id.as_str()) != id)
            .map(|w| w.name.as_str())
            .collect();
        let name = resolve_name_conflict(name, &taken);
        let now = now_ms();
        let active = clamp_active(input.active, &input.tabs);

        let saved = match id.and_then(|id| file.workspaces.iter_mut().find(|w| w.id == id)) {
            Some(existing) => {
                existing.name = name;
                existing.tabs = input.tabs;
                existing.active = active;
                existing.modified_ms = now;
                existing.clone()
            }
            None => {
                let w = Workspace {
                    id: new_id(),
                    name,
                    tabs: input.tabs,
                    active,
                    created_ms: now,
                    modified_ms: now,
                };
                file.workspaces.push(w.clone());
                w
            }
        };
        write_json(&self.file.path, &file)?;
        Ok(saved)
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<Workspace> {
        let name = name.trim();
        if name.is_empty() {
            return Err(CoreError::Other("workspace name cannot be empty".into()));
        }
        let _g = self.file.guard();
        let mut file: WorkspaceFile = read_json(&self.file.path)?;
        let taken: Vec<&str> = file
            .workspaces
            .iter()
            .filter(|w| w.id != id)
            .map(|w| w.name.as_str())
            .collect();
        let name = resolve_name_conflict(name, &taken);
        let w = file
            .workspaces
            .iter_mut()
            .find(|w| w.id == id)
            .ok_or_else(|| CoreError::Other(format!("no workspace with id `{id}`")))?;
        w.name = name;
        w.modified_ms = now_ms();
        let renamed = w.clone();
        write_json(&self.file.path, &file)?;
        Ok(renamed)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let _g = self.file.guard();
        let mut file: WorkspaceFile = read_json(&self.file.path)?;
        file.workspaces.retain(|w| w.id != id);
        write_json(&self.file.path, &file)
    }

    /// The tabs that were open when the app last ran, if any were recorded.
    pub fn last(&self) -> Result<Option<Workspace>> {
        let _g = self.file.guard();
        let file: WorkspaceFile = read_json(&self.file.path)?;
        Ok(file.last)
    }

    /// Records the currently open tabs. Unlike a named save this never fails
    /// on a tab kind it doesn't know (a newer UI may have added one): such
    /// tabs are skipped so the rest still restore.
    pub fn set_last(&self, tabs: Vec<WorkspaceTab>, active: usize) -> Result<Workspace> {
        let _g = self.file.guard();
        let mut file: WorkspaceFile = read_json(&self.file.path)?;
        // Re-map `active` onto the filtered list so it still points at the same tab.
        let active_tab = tabs.get(active).filter(|t| known_tab_kind(&t.kind)).cloned();
        let tabs: Vec<WorkspaceTab> = tabs.into_iter().filter(|t| known_tab_kind(&t.kind)).collect();
        let active = active_tab.and_then(|a| tabs.iter().position(|t| *t == a)).unwrap_or(0);
        let now = now_ms();
        let last = Workspace {
            id: "last".into(),
            name: "Last session".into(),
            active: clamp_active(active, &tabs),
            tabs,
            created_ms: file.last.as_ref().map_or(now, |l| l.created_ms),
            modified_ms: now,
        };
        file.last = Some(last.clone());
        write_json(&self.file.path, &file)?;
        Ok(last)
    }
}

/// A compare that was closed without being saved (an auto-saved session).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentSession {
    pub id: String,
    pub kind: String,
    pub left: String,
    pub right: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default)]
    pub settings: serde_json::Value,
    pub closed_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentInput {
    pub kind: String,
    #[serde(default)]
    pub left: String,
    #[serde(default)]
    pub right: String,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub settings: serde_json::Value,
}

impl RecentSession {
    /// Two entries are the same compare when they look at the same paths.
    fn same_target(&self, input: &RecentInput) -> bool {
        let opt = |o: &Option<String>| o.clone().unwrap_or_default();
        self.kind == input.kind
            && self.left == input.left
            && self.right == input.right
            && opt(&self.base) == opt(&input.base)
            && opt(&self.output) == opt(&input.output)
    }
}

#[derive(Default, Serialize, Deserialize)]
struct RecentFile {
    #[serde(default)]
    recent: Vec<RecentSession>,
}

pub struct RecentStore {
    file: JsonFile,
}

impl RecentStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            file: JsonFile::new(path),
        }
    }

    /// Most recently closed first.
    pub fn list(&self) -> Result<Vec<RecentSession>> {
        let _g = self.file.guard();
        let mut file: RecentFile = read_json(&self.file.path)?;
        file.recent.sort_by_key(|r| std::cmp::Reverse(r.closed_ms));
        Ok(file.recent)
    }

    /// Remembers a closed compare. Closing the same compare again replaces
    /// its entry (with the newer settings) and moves it to the top; the
    /// oldest entries fall off past [`RECENT_LIMIT`].
    pub fn add(&self, input: RecentInput) -> Result<RecentSession> {
        if !SESSION_KINDS.contains(&input.kind.as_str()) {
            return Err(CoreError::Other(format!("unknown session kind `{}`", input.kind)));
        }
        if input.left.is_empty() && input.right.is_empty() {
            return Err(CoreError::Other("nothing to remember: both paths are empty".into()));
        }
        let _g = self.file.guard();
        let mut file: RecentFile = read_json(&self.file.path)?;
        file.recent.retain(|r| !r.same_target(&input));
        // Strictly increasing timestamps keep the order stable for fast repeated closes.
        let newest = file.recent.iter().map(|r| r.closed_ms).max().unwrap_or(0);
        let entry = RecentSession {
            id: new_id(),
            kind: input.kind,
            left: input.left,
            right: input.right,
            base: input.base.filter(|s| !s.is_empty()),
            output: input.output.filter(|s| !s.is_empty()),
            settings: input.settings,
            closed_ms: now_ms().max(newest + 1),
        };
        file.recent.insert(0, entry.clone());
        file.recent.sort_by_key(|r| std::cmp::Reverse(r.closed_ms));
        file.recent.truncate(RECENT_LIMIT);
        write_json(&self.file.path, &file)?;
        Ok(entry)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let _g = self.file.guard();
        let mut file: RecentFile = read_json(&self.file.path)?;
        file.recent.retain(|r| r.id != id);
        write_json(&self.file.path, &file)
    }

    pub fn clear(&self) -> Result<()> {
        let _g = self.file.guard();
        write_json(&self.file.path, &RecentFile::default())
    }
}

/// App-wide preferences: one JSON object whose shape belongs to the UI.
/// Kept in the config directory (not webview storage) so it survives
/// updates and reinstalls.
pub struct PrefsStore {
    file: JsonFile,
}

impl PrefsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            file: JsonFile::new(path),
        }
    }

    /// The stored preferences, or an empty object before the first save.
    pub fn load(&self) -> Result<serde_json::Value> {
        let _g = self.file.guard();
        let value: Option<serde_json::Value> = read_json(&self.file.path)?;
        Ok(match value {
            Some(v @ serde_json::Value::Object(_)) => v,
            _ => serde_json::Value::Object(Default::default()),
        })
    }

    pub fn save(&self, prefs: serde_json::Value) -> Result<()> {
        if !prefs.is_object() {
            return Err(CoreError::Other("preferences must be a JSON object".into()));
        }
        let _g = self.file.guard();
        write_json(&self.file.path, &prefs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tab(kind: &str, left: &str, right: &str) -> WorkspaceTab {
        WorkspaceTab {
            kind: kind.into(),
            left: left.into(),
            right: right.into(),
            base: None,
            output: None,
            settings: json!({ "ignoreCase": true }),
            session_id: None,
            session_name: None,
        }
    }

    fn ws(name: &str, tabs: Vec<WorkspaceTab>, active: usize) -> WorkspaceInput {
        WorkspaceInput {
            id: None,
            name: name.into(),
            tabs,
            active,
        }
    }

    fn recent(kind: &str, left: &str, right: &str) -> RecentInput {
        RecentInput {
            kind: kind.into(),
            left: left.into(),
            right: right.into(),
            base: None,
            output: None,
            settings: json!({}),
        }
    }

    #[test]
    fn workspace_save_list_update_rename_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path().join("workspaces.json"));
        assert!(store.list().unwrap().is_empty());

        let mut linked = tab("merge", "/l", "/r");
        linked.base = Some("/b".into());
        linked.output = Some("/o".into());
        linked.session_id = Some("abc".into());
        linked.session_name = Some("Nightly".into());
        let a = store
            .save(ws("Release", vec![tab("home", "", ""), linked.clone()], 1))
            .unwrap();
        assert_eq!(a.active, 1);
        assert_eq!(a.tabs[1], linked);

        // Overwriting keeps id and creation time.
        let mut upd = ws("Release", vec![tab("text", "/a", "/b")], 7);
        upd.id = Some(a.id.clone());
        let a2 = store.save(upd).unwrap();
        assert_eq!(a2.id, a.id);
        assert_eq!(a2.created_ms, a.created_ms);
        assert_eq!(a2.active, 0, "active is clamped to the tab list");

        // Names stay unique, like saved sessions.
        let b = store.save(ws("release", vec![tab("folder", "/x", "/y")], 0)).unwrap();
        assert_eq!(b.name, "release (2)");
        assert_eq!(store.rename(&b.id, "Release").unwrap().name, "Release (2)");
        assert_eq!(store.rename(&b.id, "Hotfix").unwrap().name, "Hotfix");
        assert!(store.rename("missing", "x").is_err());

        assert_eq!(store.list().unwrap().len(), 2);
        store.delete(&a.id).unwrap();
        let all = store.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "Hotfix");
    }

    #[test]
    fn workspace_rejects_blank_names_empty_and_unknown_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path().join("w.json"));
        assert!(store.save(ws("  ", vec![tab("text", "a", "b")], 0)).is_err());
        assert!(store.save(ws("x", vec![], 0)).is_err());
        assert!(store.save(ws("x", vec![tab("bogus", "a", "b")], 0)).is_err());
        assert!(store.save(ws("x", vec![tab("profiles", "", "")], 0)).is_ok());
    }

    #[test]
    fn last_workspace_is_separate_and_skips_unknown_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.json");
        let store = WorkspaceStore::new(&path);
        assert!(store.last().unwrap().is_none());

        let tabs = vec![
            tab("future", "a", "b"),
            tab("text", "/1", "/2"),
            tab("folder", "/3", "/4"),
        ];
        let last = store.set_last(tabs, 2).unwrap();
        assert_eq!(last.tabs.len(), 2);
        assert_eq!(last.tabs[last.active].kind, "folder", "active follows its tab");
        // Not listed among named workspaces, and survives a fresh store.
        assert!(store.list().unwrap().is_empty());
        let reopened = WorkspaceStore::new(&path).last().unwrap().unwrap();
        assert_eq!(reopened.tabs, last.tabs);

        let first = store.set_last(vec![tab("home", "", "")], 0).unwrap();
        assert_eq!(first.created_ms, last.created_ms);
        assert_eq!(store.last().unwrap().unwrap().tabs.len(), 1);
    }

    #[test]
    fn recent_dedups_moves_to_top_and_caps() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecentStore::new(dir.path().join("recent.json"));
        store.add(recent("text", "/a", "/b")).unwrap();
        store.add(recent("folder", "/x", "/y")).unwrap();
        let mut again = recent("text", "/a", "/b");
        again.settings = json!({ "ignoreCase": true });
        let top = store.add(again).unwrap();

        let all = store.list().unwrap();
        assert_eq!(all.len(), 2, "closing the same compare twice keeps one entry");
        assert_eq!(all[0].id, top.id);
        assert_eq!(all[0].settings["ignoreCase"], true);
        assert_eq!(all[1].kind, "folder");

        // A different kind over the same paths is a different compare.
        store.add(recent("hex", "/a", "/b")).unwrap();
        assert_eq!(store.list().unwrap().len(), 3);

        for i in 0..RECENT_LIMIT + 5 {
            store.add(recent("text", &format!("/l{i}"), "/r")).unwrap();
        }
        let all = store.list().unwrap();
        assert_eq!(all.len(), RECENT_LIMIT);
        assert_eq!(all[0].left, format!("/l{}", RECENT_LIMIT + 4));
        assert!(all.windows(2).all(|w| w[0].closed_ms > w[1].closed_ms));

        store.remove(&all[0].id).unwrap();
        assert_eq!(store.list().unwrap().len(), RECENT_LIMIT - 1);
        store.clear().unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn recent_rejects_shell_tabs_and_empty_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecentStore::new(dir.path().join("recent.json"));
        assert!(store.add(recent("home", "/a", "/b")).is_err());
        assert!(store.add(recent("text", "", "")).is_err());
        let mut merge = recent("merge", "/l", "/r");
        merge.base = Some(String::new());
        let saved = store.add(merge).unwrap();
        assert_eq!(saved.base, None, "empty optional paths are dropped");
        assert!(store.add(recent("text", "/only-left", "")).is_ok());
    }

    #[test]
    fn prefs_roundtrip_and_tolerate_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefs.json");
        let store = PrefsStore::new(&path);
        assert_eq!(store.load().unwrap(), json!({}));
        store
            .save(json!({ "theme": "light", "keys": { "app.help": "F1" } }))
            .unwrap();
        assert_eq!(PrefsStore::new(&path).load().unwrap()["theme"], "light");
        assert!(store.save(json!([1, 2])).is_err());

        // A hand-edited file holding something other than an object reads as empty.
        std::fs::write(&path, "42").unwrap();
        assert_eq!(store.load().unwrap(), json!({}));
    }
}
