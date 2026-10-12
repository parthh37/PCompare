//! Named, saved comparison sessions.
//!
//! A saved session records what to compare (kind + both paths) plus the
//! session's view settings. Settings are an opaque JSON blob owned by the UI:
//! the engine never interprets them, so adding a new toggle to a session view
//! doesn't require a Rust change or a file-format migration.

use crate::store::{new_id, now_ms, read_json, write_json};
use crate::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

pub const SESSION_KINDS: &[&str] = &[
    "text",
    "merge",
    "table",
    "picture",
    "folder",
    "sync",
    "folderMerge",
    "hex",
    "registry",
    "version",
    "mp3",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedSession {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub left: String,
    pub right: String,
    /// Common ancestor, for 3-way merge sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Merge destination, for 3-way merge sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default)]
    pub settings: serde_json::Value,
    pub created_ms: i64,
    pub modified_ms: i64,
}

/// What the UI sends to save: `id` set = update that session, else create.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInput {
    pub id: Option<String>,
    pub name: String,
    pub kind: String,
    pub left: String,
    pub right: String,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub settings: serde_json::Value,
}

#[derive(Default, Serialize, Deserialize)]
struct SessionFile {
    #[serde(default)]
    sessions: Vec<SavedSession>,
}

pub struct SessionStore {
    path: PathBuf,
    // Serializes read-modify-write cycles from concurrent IPC calls.
    lock: Mutex<()>,
}

/// Decides the final name when `desired` is already used by another session.
///
/// `taken` holds the names of all *other* sessions (the one being updated is
/// excluded). Like Explorer, a clash gets the first free ` (n)` suffix, so a
/// save never silently replaces another session.
fn resolve_name_conflict(desired: &str, taken: &[&str]) -> String {
    let free = |name: &str| !taken.iter().any(|t| t.eq_ignore_ascii_case(name));
    if free(desired) {
        return desired.to_owned();
    }
    (2..)
        .map(|n| format!("{desired} ({n})"))
        .find(|name| free(name))
        .expect("finitely many names are taken")
}

impl SessionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// All sessions, most recently saved first.
    pub fn list(&self) -> Result<Vec<SavedSession>> {
        let _g = self.guard();
        let mut file: SessionFile = read_json(&self.path)?;
        file.sessions.sort_by_key(|s| std::cmp::Reverse(s.modified_ms));
        Ok(file.sessions)
    }

    pub fn save(&self, input: SessionInput) -> Result<SavedSession> {
        let name = input.name.trim();
        if name.is_empty() {
            return Err(CoreError::Other("session name cannot be empty".into()));
        }
        if !SESSION_KINDS.contains(&input.kind.as_str()) {
            return Err(CoreError::Other(format!("unknown session kind `{}`", input.kind)));
        }
        let _g = self.guard();
        let mut file: SessionFile = read_json(&self.path)?;
        let id = input.id.as_deref();
        let taken: Vec<&str> = file
            .sessions
            .iter()
            .filter(|s| Some(s.id.as_str()) != id)
            .map(|s| s.name.as_str())
            .collect();
        let name = resolve_name_conflict(name, &taken);
        let now = now_ms();

        let saved = match id.and_then(|id| file.sessions.iter_mut().find(|s| s.id == id)) {
            Some(existing) => {
                existing.name = name;
                existing.kind = input.kind;
                existing.left = input.left;
                existing.right = input.right;
                existing.base = input.base;
                existing.output = input.output;
                existing.settings = input.settings;
                existing.modified_ms = now;
                existing.clone()
            }
            None => {
                let s = SavedSession {
                    id: new_id(),
                    name,
                    kind: input.kind,
                    left: input.left,
                    right: input.right,
                    base: input.base,
                    output: input.output,
                    settings: input.settings,
                    created_ms: now,
                    modified_ms: now,
                };
                file.sessions.push(s.clone());
                s
            }
        };
        write_json(&self.path, &file)?;
        Ok(saved)
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<SavedSession> {
        let existing = self
            .list()?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| CoreError::Other(format!("no saved session with id `{id}`")))?;
        self.save(SessionInput {
            id: Some(existing.id),
            name: name.to_owned(),
            kind: existing.kind,
            left: existing.left,
            right: existing.right,
            base: existing.base,
            output: existing.output,
            settings: existing.settings,
        })
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let _g = self.guard();
        let mut file: SessionFile = read_json(&self.path)?;
        file.sessions.retain(|s| s.id != id);
        write_json(&self.path, &file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str) -> SessionInput {
        SessionInput {
            id: None,
            name: name.into(),
            kind: "folder".into(),
            left: "/a".into(),
            right: "remote://s3/b".into(),
            base: None,
            output: None,
            settings: serde_json::json!({ "criteria": "content" }),
        }
    }

    #[test]
    fn save_list_update_delete_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions.json"));
        assert!(store.list().unwrap().is_empty());

        let a = store.save(input("Release check")).unwrap();
        let mut upd = input("Release check v2");
        upd.id = Some(a.id.clone());
        let a2 = store.save(upd).unwrap();
        assert_eq!(a2.id, a.id);
        assert_eq!(a2.created_ms, a.created_ms);

        let all = store.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "Release check v2");
        assert_eq!(all[0].settings["criteria"], "content");

        assert_eq!(store.rename(&a.id, "Renamed").unwrap().name, "Renamed");
        store.delete(&a.id).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn duplicate_names_get_a_numbered_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("s.json"));
        let a = store.save(input("Nightly")).unwrap();
        assert_eq!(store.save(input("nightly")).unwrap().name, "nightly (2)");
        assert_eq!(store.save(input("Nightly")).unwrap().name, "Nightly (3)");
        // Re-saving a session under its own name is not a clash.
        let mut same = input("Nightly");
        same.id = Some(a.id.clone());
        assert_eq!(store.save(same).unwrap().name, "Nightly");
        assert_eq!(store.rename(&a.id, "Nightly (2)").unwrap().name, "Nightly (2) (2)");
    }

    #[test]
    fn rejects_blank_names_and_unknown_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("s.json"));
        assert!(store.save(input("   ")).is_err());
        let mut bad = input("x");
        bad.kind = "home".into();
        assert!(store.save(bad).is_err());
    }

    #[test]
    fn corrupt_file_is_moved_aside_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        std::fs::write(&path, "{ not json").unwrap();
        let store = SessionStore::new(&path);
        assert!(store.list().unwrap().is_empty());
        let aside = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains("corrupt"));
        assert!(aside, "original file should be preserved");
    }
}
