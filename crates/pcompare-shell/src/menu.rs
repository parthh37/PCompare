//! The Explorer right-click menu, as classic static verbs under
//! `HKCU\Software\Classes`:
//!
//! * "Select Left Side for Compare" remembers an item (`--select-left`),
//! * "Compare to 'name'" pairs another item with it (`--compare-to`),
//! * "Compare with PCompare" compares the 2 or 3 selected items at once
//!   (`--compare-selection`, grouped by [`crate::selection`]).
//!
//! The same verbs exist on the background of a folder window (`%V` = that
//! folder). Windows 11 lists classic verbs under "Show more options".

use serde::Serialize;

/// Keys PCompare owns, relative to `HKCU\Software\Classes`.
const ITEM_ROOTS: [&str; 2] = ["*", "Directory"];
const BACKGROUND_ROOT: &str = r"Directory\Background";

const SELECT_LEFT: &str = "PCompare.SelectLeft";
const COMPARE_TO: &str = "PCompare.CompareTo";
const COMPARE: &str = "PCompare.Compare";

/// Where the "don't add the menu" choice lives, so the installer, the app's
/// startup check and Settings all agree without reading the app's prefs file.
#[cfg_attr(not(windows), allow(dead_code))]
const SETTINGS_KEY: &str = r"Software\PCompare\ShellMenu";
#[cfg_attr(not(windows), allow(dead_code))]
const DISABLED_VALUE: &str = "Disabled";

/// One registry key to create and the string values to put in it
/// (`None` = the key's default value).
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Relative to `HKCU`.
    pub key: String,
    pub values: Vec<(Option<&'static str>, String)>,
}

fn verb(
    root: &str,
    name: &str,
    label: String,
    icon: &str,
    command: String,
    multi: bool,
) -> [Entry; 2] {
    let key = format!(r"Software\Classes\{root}\shell\{name}");
    let mut values = vec![(None, label), (Some("Icon"), icon.to_owned())];
    if multi {
        // One verb for any number of selected items (Explorer then starts one
        // process per item; `selection` groups them back together).
        values.push((Some("MultiSelectModel"), "Player".to_owned()));
    }
    [
        Entry {
            key: key.clone(),
            values,
        },
        Entry {
            key: format!(r"{key}\command"),
            values: vec![(None, command)],
        },
    ]
}

/// "Compare to …" names what it will compare against.
pub fn compare_to_label(left: Option<&str>, background: bool) -> String {
    let name = left.map(|p| {
        let trimmed = p.trim_end_matches(['\\', '/']);
        trimmed
            .rsplit(['\\', '/'])
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or(trimmed)
            .to_owned()
    });
    match (name, background) {
        (Some(n), false) => format!("Compare to '{n}'"),
        (Some(n), true) => format!("Compare This Folder to '{n}'"),
        (None, false) => "Compare to Left Side".to_owned(),
        (None, true) => "Compare This Folder to Left Side".to_owned(),
    }
}

/// Every key and value the menu consists of, for the app at `exe`.
pub fn entries(exe: &str, left: Option<&str>) -> Vec<Entry> {
    let icon = format!("\"{exe}\",0");
    let run = |args: &str| format!("\"{exe}\" {args}");
    let mut out = Vec::new();
    for root in ITEM_ROOTS {
        out.extend(verb(
            root,
            SELECT_LEFT,
            "Select Left Side for Compare".into(),
            &icon,
            run(r#"--select-left "%1""#),
            false,
        ));
        out.extend(verb(
            root,
            COMPARE_TO,
            compare_to_label(left, false),
            &icon,
            run(r#"--compare-to "%1""#),
            false,
        ));
        out.extend(verb(
            root,
            COMPARE,
            "Compare with PCompare".into(),
            &icon,
            run(r#"--compare-selection "%1""#),
            true,
        ));
    }
    let bg = BACKGROUND_ROOT;
    out.extend(verb(
        bg,
        SELECT_LEFT,
        "Select This Folder as Left Side".into(),
        &icon,
        run(r#"--select-left "%V""#),
        false,
    ));
    out.extend(verb(
        bg,
        COMPARE_TO,
        compare_to_label(left, true),
        &icon,
        run(r#"--compare-to "%V""#),
        false,
    ));
    out
}

/// The verb keys to delete when removing the menu (each with its subkeys).
#[cfg_attr(not(windows), allow(dead_code))]
fn owned_keys() -> Vec<String> {
    let mut keys = Vec::new();
    for root in ITEM_ROOTS {
        for name in [SELECT_LEFT, COMPARE_TO, COMPARE] {
            keys.push(format!(r"Software\Classes\{root}\shell\{name}"));
        }
    }
    for name in [SELECT_LEFT, COMPARE_TO] {
        keys.push(format!(r"Software\Classes\{BACKGROUND_ROOT}\shell\{name}"));
    }
    keys
}

/// The command a verb runs, used to tell whether the menu points at this copy.
#[cfg_attr(not(windows), allow(dead_code))]
fn probe(exe: &str) -> (String, String) {
    let e = entries(exe, None);
    let cmd = e
        .iter()
        .find(|e| e.key.ends_with(r"Directory\shell\PCompare.Compare\command"))
        .expect("verb exists");
    (cmd.key.clone(), cmd.values[0].1.clone())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum MenuStatus {
    /// The menu is there and runs this copy of PCompare.
    Installed,
    /// The menu runs another copy (moved, reinstalled elsewhere, or a dev build).
    Stale { exe: String },
    /// No menu, and the user didn't turn it off (e.g. an MSI install before first launch).
    Missing,
    /// Turned off in Settings; startup won't add it back.
    Disabled,
    /// Not Windows.
    Unsupported,
}

pub use imp::*;

#[cfg(not(windows))]
mod imp {
    use super::MenuStatus;
    use std::io;

    pub fn status(_exe: &str) -> MenuStatus {
        MenuStatus::Unsupported
    }
    pub fn register(_exe: &str, _left: Option<&str>) -> io::Result<()> {
        Ok(())
    }
    pub fn unregister() -> io::Result<()> {
        Ok(())
    }
    pub fn set_disabled(_disabled: bool) -> io::Result<()> {
        Ok(())
    }
    pub fn ensure(_exe: &str, _left: Option<&str>) -> io::Result<bool> {
        Ok(false)
    }
    pub fn set_left(_left: Option<&str>) {}
}

#[cfg(windows)]
mod imp {
    use super::{
        compare_to_label, entries, owned_keys, probe, MenuStatus, BACKGROUND_ROOT, COMPARE_TO,
        DISABLED_VALUE, ITEM_ROOTS, SETTINGS_KEY,
    };
    use std::io;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteTreeW, RegGetValueW, RegOpenKeyExW,
        RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, KEY_WRITE, REG_DWORD,
        REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
    };
    use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn check(code: u32) -> io::Result<()> {
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code as i32))
        }
    }

    /// An open key, closed on drop.
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn create(path: &str) -> io::Result<Key> {
        let mut key: HKEY = null_mut();
        let path = wide(path);
        check(unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                null(),
                &mut key,
                null_mut(),
            )
        })?;
        Ok(Key(key))
    }

    /// Opens an existing key for writing values; `None` if it isn't there.
    fn open_existing(path: &str) -> Option<Key> {
        let mut key: HKEY = null_mut();
        let path = wide(path);
        let code =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_SET_VALUE, &mut key) };
        (code == ERROR_SUCCESS).then_some(Key(key))
    }

    fn set_string(key: &Key, name: Option<&str>, value: &str) -> io::Result<()> {
        let name = name.map(wide);
        let data = wide(value);
        check(unsafe {
            RegSetValueExW(
                key.0,
                name.as_ref().map_or(null(), |n| n.as_ptr()),
                0,
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        })
    }

    fn get_string(path: &str, name: Option<&str>) -> Option<String> {
        let path = wide(path);
        let name = name.map(wide);
        let name_ptr = name.as_ref().map_or(null(), |n| n.as_ptr());
        let mut size = 0u32;
        let code = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                name_ptr,
                RRF_RT_REG_SZ,
                null_mut(),
                null_mut(),
                &mut size,
            )
        };
        if code != ERROR_SUCCESS || size == 0 {
            return None;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        let code = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                name_ptr,
                RRF_RT_REG_SZ,
                null_mut(),
                buf.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if code != ERROR_SUCCESS {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    fn is_disabled() -> bool {
        let path = wide(SETTINGS_KEY);
        let name = wide(DISABLED_VALUE);
        let mut value = 0u32;
        let mut size = 4u32;
        let code = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                null_mut(),
                (&mut value as *mut u32).cast(),
                &mut size,
            )
        };
        code == ERROR_SUCCESS && value != 0
    }

    /// Tells Explorer the verbs changed so open windows pick them up.
    fn notify_shell() {
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, null(), null()) };
    }

    pub fn status(exe: &str) -> MenuStatus {
        if is_disabled() {
            return MenuStatus::Disabled;
        }
        let (key, expected) = probe(exe);
        match get_string(&key, None) {
            None => MenuStatus::Missing,
            Some(cmd) if cmd.eq_ignore_ascii_case(&expected) => MenuStatus::Installed,
            Some(cmd) => {
                // `"C:\…\PCompare.exe" --compare-selection "%1"` → the exe path.
                let exe = cmd
                    .trim_start_matches('"')
                    .split('"')
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                MenuStatus::Stale { exe }
            }
        }
    }

    /// Writes every verb for `exe` (overwriting another copy's) and clears the opt-out.
    pub fn register(exe: &str, left: Option<&str>) -> io::Result<()> {
        for entry in entries(exe, left) {
            let key = create(&entry.key)?;
            for (name, value) in &entry.values {
                set_string(&key, *name, value)?;
            }
        }
        notify_shell();
        Ok(())
    }

    /// Deletes `path` and everything under it; a missing key is fine.
    fn delete_key(path: &str) -> io::Result<()> {
        let path = wide(path);
        let code = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, path.as_ptr()) };
        if code != ERROR_FILE_NOT_FOUND {
            check(code)?;
        }
        // RegDeleteTreeW empties the key; this removes the key itself.
        let code = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, path.as_ptr()) };
        if code != ERROR_FILE_NOT_FOUND {
            check(code)?;
        }
        Ok(())
    }

    /// Removes every verb PCompare owns; missing keys are fine.
    pub fn unregister() -> io::Result<()> {
        for key in owned_keys() {
            delete_key(&key)?;
        }
        notify_shell();
        Ok(())
    }

    /// Records the Settings choice; turning the menu off also removes it.
    pub fn set_disabled(disabled: bool) -> io::Result<()> {
        let key = create(SETTINGS_KEY)?;
        let value = u32::from(disabled);
        check(unsafe {
            RegSetValueExW(
                key.0,
                wide(DISABLED_VALUE).as_ptr(),
                0,
                REG_DWORD,
                (&value as *const u32).cast(),
                4,
            )
        })?;
        if disabled {
            unregister()?;
        }
        Ok(())
    }

    /// Startup check: (re)writes the menu unless the user turned it off or it
    /// already points here. Returns whether anything was written.
    pub fn ensure(exe: &str, left: Option<&str>) -> io::Result<bool> {
        match status(exe) {
            MenuStatus::Installed | MenuStatus::Disabled | MenuStatus::Unsupported => Ok(false),
            MenuStatus::Missing | MenuStatus::Stale { .. } => register(exe, left).map(|_| true),
        }
    }

    /// Relabels "Compare to …" after the left side changes. Only touches keys
    /// that exist, so it never brings back a menu the user removed.
    pub fn set_left(left: Option<&str>) {
        let mut changed = false;
        for root in ITEM_ROOTS {
            if let Some(key) =
                open_existing(&format!(r"Software\Classes\{root}\shell\{COMPARE_TO}"))
            {
                changed |= set_string(&key, None, &compare_to_label(left, false)).is_ok();
            }
        }
        if let Some(key) = open_existing(&format!(
            r"Software\Classes\{BACKGROUND_ROOT}\shell\{COMPARE_TO}"
        )) {
            changed |= set_string(&key, None, &compare_to_label(left, true)).is_ok();
        }
        if changed {
            notify_shell();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Users\me\AppData\Local\PCompare\pcompare.exe";

    fn value<'a>(all: &'a [Entry], key_suffix: &str) -> &'a str {
        &all.iter()
            .find(|e| e.key.ends_with(key_suffix))
            .unwrap_or_else(|| panic!("{key_suffix}"))
            .values[0]
            .1
    }

    #[test]
    fn verbs_quote_the_exe_and_the_item() {
        let all = entries(EXE, None);
        assert_eq!(
            value(&all, r"*\shell\PCompare.SelectLeft\command"),
            format!(r#""{EXE}" --select-left "%1""#)
        );
        assert_eq!(
            value(&all, r"Directory\shell\PCompare.Compare\command"),
            format!(r#""{EXE}" --compare-selection "%1""#)
        );
        assert_eq!(
            value(&all, r"Background\shell\PCompare.CompareTo\command"),
            format!(r#""{EXE}" --compare-to "%V""#)
        );
        // The multi-select verb is the only one that opts into any number of items.
        let multi: Vec<_> = all
            .iter()
            .filter(|e| e.values.iter().any(|(n, _)| *n == Some("MultiSelectModel")))
            .collect();
        assert_eq!(multi.len(), 2);
        assert!(multi.iter().all(|e| e.key.ends_with("PCompare.Compare")));
    }

    #[test]
    fn every_written_verb_is_removed_on_unregister() {
        let owned = owned_keys();
        for e in entries(EXE, None) {
            let verb_key = e.key.trim_end_matches(r"\command");
            assert!(
                owned.iter().any(|k| k == verb_key),
                "{verb_key} would be left behind"
            );
        }
    }

    #[test]
    fn compare_to_names_the_left_side() {
        assert_eq!(
            compare_to_label(Some(r"C:\src\v1\"), false),
            "Compare to 'v1'"
        );
        assert_eq!(
            compare_to_label(Some("/tmp/a.txt"), true),
            "Compare This Folder to 'a.txt'"
        );
        assert_eq!(compare_to_label(Some(r"C:\"), false), r"Compare to 'C:'");
        assert_eq!(compare_to_label(None, false), "Compare to Left Side");
        let all = entries(EXE, Some(r"D:\old"));
        assert_eq!(
            value(&all, r"*\shell\PCompare.CompareTo"),
            "Compare to 'old'"
        );
    }
}
