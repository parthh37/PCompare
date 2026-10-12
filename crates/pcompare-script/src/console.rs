//! Console output for a GUI-subsystem executable.
//!
//! Release builds on Windows are linked as GUI programs (no console window),
//! so `pcompare @script.txt` from cmd.exe or PowerShell would print nothing.
//! [`attach_parent`] attaches to the console of the shell that started us,
//! unless output is already redirected (`> log.txt`, a pipe).

/// Attaches stdout/stderr to the parent process's console on Windows when
/// they aren't connected to anything yet. A no-op elsewhere.
#[cfg(windows)]
pub fn attach_parent() {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
    }
    // (DWORD)-1, (DWORD)-11 and (DWORD)-12 from the Windows headers.
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    const STD_OUTPUT_HANDLE: u32 = u32::MAX - 10;
    const STD_ERROR_HANDLE: u32 = u32::MAX - 11;

    let unset = |h: *mut c_void| h.is_null() || h as isize == -1;
    // SAFETY: plain Win32 calls without pointer arguments; a failed attach
    // (no parent console, e.g. started from Explorer) just leaves output unseen.
    unsafe {
        if unset(GetStdHandle(STD_OUTPUT_HANDLE)) && unset(GetStdHandle(STD_ERROR_HANDLE)) {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

#[cfg(not(windows))]
pub fn attach_parent() {}
