//! Reading the live registry through `reg.exe` (Windows only).
//!
//! Local keys are exported with `reg export`, which preserves exact value
//! types. Remote machines (`\\MACHINE\HKLM\...`) only support `reg query`,
//! which requires the Remote Registry service on the target and suitable
//! permissions; only HKLM and HKU are reachable remotely.

use super::{LiveSpec, RegTree};
use pcompare_core::{CoreError, Result};

#[cfg(not(windows))]
pub fn load_live(_spec: &LiveSpec) -> Result<RegTree> {
    Err(CoreError::Other(
        "Live registry is only available on Windows; compare .reg exports instead".into(),
    ))
}

#[cfg(windows)]
pub fn load_live(spec: &LiveSpec) -> Result<RegTree> {
    match spec.machine {
        None => export_local(spec),
        Some(_) => query_remote(spec),
    }
}

#[cfg(windows)]
fn reg_exe(args: &[&str]) -> Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // Resolve reg.exe from System32 so a stray reg.exe on PATH is never used.
    let exe = std::env::var_os("SystemRoot")
        .map(|root| {
            std::path::PathBuf::from(root)
                .join("System32")
                .join("reg.exe")
        })
        .filter(|p| p.is_file())
        .unwrap_or_else(|| "reg.exe".into());
    let output = std::process::Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| CoreError::Other(format!("cannot run reg.exe: {e}")))?;
    if !output.status.success() {
        let stderr = pcompare_core::textfile::decode(&output.stderr).0;
        let stdout = pcompare_core::textfile::decode(&output.stdout).0;
        let message = [stderr.trim(), stdout.trim()]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("reg.exe failed");
        return Err(CoreError::Other(message.to_owned()));
    }
    Ok(output)
}

/// Deletes the temporary export however we leave `export_local`.
#[cfg(windows)]
struct TempFile(std::path::PathBuf);

#[cfg(windows)]
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(windows)]
fn export_local(spec: &LiveSpec) -> Result<RegTree> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let tmp = TempFile(std::env::temp_dir().join(format!(
        "pcompare-reg-{}-{}.reg",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    let key = spec.reg_arg();
    let target = tmp.0.to_string_lossy().into_owned();
    reg_exe(&["export", &key, &target, "/y"])
        .map_err(|e| CoreError::Other(format!("{}: {e}", spec.display())))?;
    super::load_file(&tmp.0).map_err(|e| match e {
        // Don't leak the temp path into the message.
        CoreError::Other(m) => CoreError::Other(m.replace(&target, &spec.display())),
        other => other,
    })
}

#[cfg(windows)]
fn query_remote(spec: &LiveSpec) -> Result<RegTree> {
    let key = spec.reg_arg();
    let output = reg_exe(&["query", &key, "/s"]).map_err(|e| {
        CoreError::Other(format!(
            "{}: {e} (remote registry access needs the Remote Registry service running on the target and permission to read the key)",
            spec.display()
        ))
    })?;
    let text = pcompare_core::textfile::decode(&output.stdout).0;
    Ok(super::parse_reg_query(&text))
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn live_is_windows_only() {
        let spec = super::super::parse_live_spec("HKCU\\Software").unwrap();
        let err = load_live(&spec).unwrap_err().to_string();
        assert!(err.contains("only available on Windows"));
    }
}
