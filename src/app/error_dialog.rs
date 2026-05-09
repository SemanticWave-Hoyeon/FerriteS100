//! Native error dialog for fatal startup errors.
//!
//! Release Windows builds run as `windows_subsystem = "windows"` and have no
//! console, so a panic or `Err` returned from `main()` would otherwise be
//! invisible. This module surfaces them via `MessageBoxW`.
//!
//! On debug builds and non-Windows platforms the function is a no-op — the
//! caller is expected to write to stderr instead.

#[cfg(all(windows, not(debug_assertions)))]
pub fn show_error_dialog(title: &str, message: &str) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let wide_title: Vec<u16> = std::ffi::OsStr::new(title)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let wide_msg: Vec<u16> = std::ffi::OsStr::new(message)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: pointers are null-terminated UTF-16 buffers we own for the
    // duration of the call; HWND null parents the dialog to the desktop.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut() as _,
            wide_msg.as_ptr(),
            wide_title.as_ptr(),
            MB_ICONERROR | MB_OK,
        );
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
#[allow(dead_code)] // Callers gate invocation behind the same cfg, so this
                    // variant is never reached on debug/non-Windows builds.
                    // Provided so the symbol resolves on every platform.
pub fn show_error_dialog(_title: &str, _message: &str) {}
