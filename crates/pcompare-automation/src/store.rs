//! `automation.json` in the app config dir: the saved tasks and the
//! automation settings. Written atomically (temp file + rename) so a crash
//! mid-save never leaves a truncated file; a file that fails to parse is
//! moved aside (`automation.json.bad-<unix_ms>`) and reported, never
//! silently replaced.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use pcompare_core::store::{new_id, now_ms};
use serde::{Deserialize, Serialize};

use crate::task::{migrate_task, normalize_task, validate_task, SyncTask, TaskIssue};
use crate::{AutomationError, Result};

/// The file format this build writes. A file with a higher version came
/// from a newer PCompare and is neither read nor overwritten.
///
/// 2: tasks carry a sync `matrix` (bc5:sync-matrix); format 1 tasks get
/// their mode's preset when read.
pub const FILE_VERSION: u32 = 2;
/// `max_parallel` is kept within this range.
pub const MAX_PARALLEL_LIMIT: u32 = 16;

/// App-wide automation settings.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct AutomationSettings {
    /// Closing the window keeps PCompare running in the tray (menu bar on
    /// macOS) so automations keep running. Quit from the tray menu.
    pub run_in_background: bool,
    /// Start PCompare (hidden, in the tray) when the user logs in.
    pub start_at_login: bool,
    /// No automatic triggers fire while paused; Run now still works.
    pub paused: bool,
    /// How many tasks may run at the same time.
    pub max_parallel: u32,
    /// Run history older than this is pruned at startup.
    pub history_days: u32,
}

impl Default for AutomationSettings {
    fn default() -> Self {
        AutomationSettings {
            run_in_background: true,
            start_at_login: false,
            paused: false,
            max_parallel: 2,
            history_days: 90,
        }
    }
}

/// The file's shape.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct AutomationFile {
    pub version: u32,
    pub settings: AutomationSettings,
    pub tasks: Vec<SyncTask>,
}

pub struct TaskStore {
    path: PathBuf,
    /// Serializes read-modify-write cycles.
    lock: Mutex<()>,
}

impl TaskStore {
    pub fn new(path: PathBuf) -> Self {
        TaskStore {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the file (a missing file is an empty default).
    ///
    /// A file that doesn't parse is moved aside to
    /// `automation.json.bad-<unix_ms>` and reported as
    /// [`AutomationError::File`]; the next load starts empty, and the bad
    /// copy is there to be fixed or restored by hand. A file written by a
    /// newer PCompare is reported and left alone.
    pub fn load(&self) -> Result<AutomationFile> {
        let _guard = self.guard();
        self.read()
    }

    /// Tasks sorted by name.
    pub fn tasks(&self) -> Result<Vec<SyncTask>> {
        let mut tasks = self.load()?.tasks;
        tasks.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(tasks)
    }

    pub fn task(&self, id: &str) -> Result<Option<SyncTask>> {
        Ok(self.load()?.tasks.into_iter().find(|t| t.id == id))
    }

    /// Validates and saves (creates when `task.id` is empty: new id,
    /// `created_ms`). On validation errors nothing is written and
    /// `AutomationError::Invalid(issues)` is returned. Returns the saved
    /// task and any warnings.
    pub fn save_task(&self, mut task: SyncTask) -> Result<(SyncTask, Vec<TaskIssue>)> {
        let _guard = self.guard();
        let mut file = self.read()?;
        task.name = task.name.trim().to_owned();
        task.left = task.left.trim().to_owned();
        task.right = task.right.trim().to_owned();
        normalize_task(&mut task);
        let others: Vec<String> = file
            .tasks
            .iter()
            .filter(|t| t.id != task.id || task.id.is_empty())
            .map(|t| t.name.clone())
            .collect();
        let issues = validate_task(&task, &others);
        if issues.iter().any(|i| i.error) {
            return Err(AutomationError::Invalid(issues));
        }
        let now = now_ms();
        task.updated_ms = now;
        if task.id.is_empty() {
            task.id = new_id();
            task.created_ms = now;
            file.tasks.push(task.clone());
        } else {
            let slot = file
                .tasks
                .iter_mut()
                .find(|t| t.id == task.id)
                .ok_or_else(|| AutomationError::TaskNotFound(task.id.clone()))?;
            task.created_ms = slot.created_ms;
            *slot = task.clone();
        }
        self.write(&file)?;
        Ok((task, issues))
    }

    pub fn delete_task(&self, id: &str) -> Result<()> {
        let _guard = self.guard();
        let mut file = self.read()?;
        let before = file.tasks.len();
        file.tasks.retain(|t| t.id != id);
        if file.tasks.len() == before {
            return Err(AutomationError::TaskNotFound(id.to_owned()));
        }
        self.write(&file)
    }

    /// Turns a task on or off without re-validating the rest of it (a task
    /// saved by an older build can still be switched off).
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<SyncTask> {
        let _guard = self.guard();
        let mut file = self.read()?;
        let task = file
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| AutomationError::TaskNotFound(id.to_owned()))?;
        task.enabled = enabled;
        task.updated_ms = now_ms();
        let task = task.clone();
        self.write(&file)?;
        Ok(task)
    }

    pub fn settings(&self) -> Result<AutomationSettings> {
        Ok(self.load()?.settings)
    }

    /// Saves the settings, keeping `max_parallel` within 1–16 and
    /// `history_days` at least 1.
    pub fn save_settings(&self, settings: &AutomationSettings) -> Result<()> {
        let _guard = self.guard();
        let mut file = self.read()?;
        file.settings = AutomationSettings {
            max_parallel: settings.max_parallel.clamp(1, MAX_PARALLEL_LIMIT),
            history_days: settings.history_days.max(1),
            ..settings.clone()
        };
        self.write(&file)
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn file_error(&self, message: String) -> AutomationError {
        AutomationError::File {
            path: self.path.display().to_string(),
            message,
        }
    }

    fn read(&self) -> Result<AutomationFile> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(AutomationFile {
                    version: FILE_VERSION,
                    ..AutomationFile::default()
                })
            }
            Err(e) => return Err(self.file_error(format!("couldn't be read: {e}"))),
        };
        // Peek at the version first so a newer file isn't mistaken for a
        // corrupt one just because its shape changed.
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&text)
        {
            if let Some(v) = map
                .get("version")
                .and_then(|v| v.as_u64())
                .filter(|&v| v > u64::from(FILE_VERSION))
            {
                return Err(self.file_error(format!(
                    "was saved by a newer version of PCompare (format {v}); update PCompare to use these automations"
                )));
            }
        }
        match serde_json::from_str::<AutomationFile>(&text) {
            Ok(mut file) => {
                for task in &mut file.tasks {
                    migrate_task(task, file.version);
                }
                file.version = FILE_VERSION;
                Ok(file)
            }
            Err(e) => {
                let mut aside = self.path.as_os_str().to_owned();
                aside.push(format!(".bad-{}", now_ms()));
                let aside = PathBuf::from(aside);
                fs::rename(&self.path, &aside).map_err(|re| {
                    self.file_error(format!(
                        "is damaged ({e}) and couldn't be moved aside: {re}"
                    ))
                })?;
                Err(self.file_error(format!(
                    "is damaged ({e}). It was moved to {} so nothing is lost; automations start empty until it's fixed and restored",
                    aside.display()
                )))
            }
        }
    }

    /// Temp file + fsync + rename, so a crash leaves either the old or the
    /// new file, never half of one.
    fn write(&self, file: &AutomationFile) -> Result<()> {
        let io_err = |e: io::Error| self.file_error(format!("couldn't be saved: {e}"));
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(io_err)?;
        }
        let file = AutomationFile {
            version: FILE_VERSION,
            ..file.clone()
        };
        let json = serde_json::to_vec_pretty(&file)
            .map_err(|e| self.file_error(format!("couldn't be saved: {e}")))?;
        let mut tmp = self.path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let result = (|| {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, &self.path)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(io_err(e));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::tests::sample;

    fn store() -> (tempfile::TempDir, TaskStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("config/automation.json"));
        (dir, store)
    }

    #[test]
    fn missing_file_is_empty() {
        let (_dir, store) = store();
        let file = store.load().unwrap();
        assert!(file.tasks.is_empty());
        assert_eq!(file.settings, AutomationSettings::default());
        assert_eq!(file.settings.max_parallel, 2);
    }

    #[test]
    fn create_update_delete() {
        let (_dir, store) = store();
        let (a, warnings) = store
            .save_task(SyncTask {
                name: "  Zeta ".into(),
                ..sample()
            })
            .unwrap();
        assert!(warnings.is_empty());
        assert!(!a.id.is_empty());
        assert_eq!(a.name, "Zeta");
        assert!(a.created_ms > 0 && a.updated_ms >= a.created_ms);
        let (b, _) = store
            .save_task(SyncTask {
                name: "alpha".into(),
                right: "/elsewhere".into(),
                ..sample()
            })
            .unwrap();
        assert_eq!(
            store
                .tasks()
                .unwrap()
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "Zeta"]
        );

        // Names are unique, but a task may keep its own name.
        let dup = store.save_task(SyncTask {
            name: "ZETA".into(),
            ..sample()
        });
        assert!(matches!(dup, Err(AutomationError::Invalid(_))));
        let (a2, _) = store
            .save_task(SyncTask {
                description: "edited".into(),
                created_ms: 0,
                ..a.clone()
            })
            .unwrap();
        assert_eq!(a2.created_ms, a.created_ms, "creation time kept");
        assert_eq!(store.task(&a.id).unwrap().unwrap().description, "edited");

        let off = store.set_enabled(&b.id, false).unwrap();
        assert!(!off.enabled);
        assert!(!store.task(&b.id).unwrap().unwrap().enabled);

        store.delete_task(&a.id).unwrap();
        assert!(store.task(&a.id).unwrap().is_none());
        assert!(matches!(
            store.delete_task(&a.id),
            Err(AutomationError::TaskNotFound(_))
        ));
        assert!(matches!(
            store.set_enabled("nope", true),
            Err(AutomationError::TaskNotFound(_))
        ));
        assert!(matches!(
            store.save_task(SyncTask {
                id: "nope".into(),
                name: "x".into(),
                ..sample()
            }),
            Err(AutomationError::TaskNotFound(_))
        ));
    }

    #[test]
    fn invalid_tasks_are_not_written() {
        let (_dir, store) = store();
        let err = store
            .save_task(SyncTask {
                left: "".into(),
                ..sample()
            })
            .unwrap_err();
        let AutomationError::Invalid(issues) = &err else {
            panic!("{err}")
        };
        assert!(issues.iter().any(|i| i.error));
        assert!(err.to_string().contains("Choose the left folder"));
        assert!(!store.path().exists());
    }

    #[test]
    fn settings_round_trip_and_clamp() {
        let (_dir, store) = store();
        store
            .save_settings(&AutomationSettings {
                paused: true,
                max_parallel: 0,
                history_days: 0,
                ..AutomationSettings::default()
            })
            .unwrap();
        let s = store.settings().unwrap();
        assert!(s.paused);
        assert_eq!((s.max_parallel, s.history_days), (1, 1));
        store
            .save_settings(&AutomationSettings {
                max_parallel: 99,
                ..s
            })
            .unwrap();
        assert_eq!(store.settings().unwrap().max_parallel, MAX_PARALLEL_LIMIT);
        // Saving a task keeps the settings.
        store.save_task(sample()).unwrap();
        assert!(store.settings().unwrap().paused);
        let text = fs::read_to_string(store.path()).unwrap();
        assert!(text.contains("\"version\": 2") && text.contains("\"maxParallel\""));
        assert!(!dir_has(store.path(), ".tmp"));
    }

    fn dir_has(path: &Path, suffix: &str) -> bool {
        fs::read_dir(path.parent().unwrap())
            .unwrap()
            .any(|e| e.unwrap().file_name().to_string_lossy().ends_with(suffix))
    }

    #[test]
    fn corrupt_file_is_moved_aside_and_reported() {
        let (_dir, store) = store();
        store.save_task(sample()).unwrap();
        let good = fs::read_to_string(store.path()).unwrap();
        fs::write(store.path(), &good[..good.len() / 2]).unwrap();

        let err = store.load().unwrap_err();
        assert!(matches!(err, AutomationError::File { .. }));
        assert!(err.to_string().contains("is damaged"), "{err}");
        let bad: Vec<PathBuf> = fs::read_dir(store.path().parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("automation.json.bad-")
            })
            .collect();
        assert_eq!(bad.len(), 1);
        assert_eq!(
            fs::read_to_string(&bad[0]).unwrap(),
            &good[..good.len() / 2],
            "kept as is"
        );
        // After reporting, the store starts empty rather than failing forever.
        assert!(store.tasks().unwrap().is_empty());
    }

    #[test]
    fn newer_file_is_refused_not_moved() {
        let (_dir, store) = store();
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), r#"{"version": 7, "tasks": "a new shape"}"#).unwrap();
        let err = store.load().unwrap_err();
        assert!(err.to_string().contains("newer version"), "{err}");
        assert!(store.save_task(sample()).is_err());
        assert!(
            fs::read_to_string(store.path())
                .unwrap()
                .contains("a new shape"),
            "untouched"
        );
    }

    #[test]
    fn format_1_tasks_get_their_mode_s_matrix() {
        use pcompare_sync::sync::{SyncAction, SyncMatrix, SyncMode};
        let (_dir, store) = store();
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(
            store.path(),
            r#"{"version": 1, "tasks": [
                {"id": "a", "name": "Mirror", "enabled": true, "left": "/l", "right": "/r", "mode": "mirrorLeftToRight"},
                {"id": "b", "name": "Two-way", "enabled": true, "left": "/l2", "right": "/r2", "mode": "twoWay"}
            ]}"#,
        )
        .unwrap();
        let tasks = store.tasks().unwrap();
        assert_eq!(tasks[0].matrix, SyncMatrix::preset(SyncMode::MirrorLeftToRight));
        assert_eq!(tasks[0].matrix.right_orphan, SyncAction::DeleteRight);
        assert_eq!(tasks[1].matrix.left_deleted, SyncAction::DeleteRight);
        assert!(!tasks[0].skip_empty_folders);

        // A custom matrix survives a save; a fixed mode's is reset to its preset.
        let mut custom = tasks[0].clone();
        custom.mode = SyncMode::Custom;
        custom.matrix.right_orphan = SyncAction::Prompt;
        custom.skip_empty_folders = true;
        let (saved, _) = store.save_task(custom).unwrap();
        assert_eq!(saved.matrix.right_orphan, SyncAction::Prompt);
        let mut fixed = tasks[1].clone();
        fixed.matrix.left_deleted = SyncAction::Leave;
        let (saved, _) = store.save_task(fixed).unwrap();
        assert_eq!(saved.matrix, SyncMatrix::preset(SyncMode::TwoWay));

        let text = fs::read_to_string(store.path()).unwrap();
        assert!(text.contains("\"version\": 2") && text.contains("\"rightOrphan\": \"prompt\""), "{text}");
        let again = store.tasks().unwrap();
        assert_eq!(again[0].mode, SyncMode::Custom);
        assert_eq!(again[0].matrix.right_orphan, SyncAction::Prompt, "format 2 isn't migrated again");
        assert!(again[0].skip_empty_folders);
    }
}
