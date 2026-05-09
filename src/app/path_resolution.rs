//! Locate the application's base directory containing `Catalogues/`.
//!
//! Resolution order is intentionally generous because the binary may be
//! launched from many places: `cargo run` (CWD = project root), Explorer
//! double-click on `target/release/ferrite-s100.exe` (CWD = exe dir), or
//! a packaged dist with the layout adjacent to the exe.

use std::path::PathBuf;

/// Get the application base directory.
///
/// Searches for a directory containing `Catalogues/` in this order:
/// 1. Executable's directory (Windows dist, Linux)
/// 2. macOS .app bundle Resources: `../Resources/` relative to executable
/// 3. Ancestors of the executable's directory (handles `target/release/` exe
///    launched from Explorer where CWD also lacks `Catalogues/`)
/// 4. Current working directory (typical for `cargo run`)
/// 5. Ancestors of the current working directory
pub fn get_app_base_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    if let Some(ref exe_dir) = exe_dir {
        // Check next to executable (Windows/Linux dist)
        if exe_dir.join("Catalogues").exists() {
            return exe_dir.clone();
        }
        // Check macOS .app bundle: Contents/MacOS/../Resources/ = Contents/Resources/
        let resources_dir = exe_dir.join("../Resources");
        if resources_dir.join("Catalogues").exists() {
            if let Ok(canonical) = resources_dir.canonicalize() {
                return canonical;
            }
            return resources_dir;
        }
        // Walk up from exe_dir looking for Catalogues/. Covers the case of
        // running the dev/release exe directly from `target/release/` via
        // File Explorer, where CWD = exe_dir and neither contains Catalogues.
        for ancestor in exe_dir.ancestors().skip(1) {
            if ancestor.join("Catalogues").exists() {
                return ancestor.to_path_buf();
            }
        }
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if cwd.join("Catalogues").exists() {
        return cwd;
    }
    // Walk up from CWD as a last resort.
    for ancestor in cwd.ancestors().skip(1) {
        if ancestor.join("Catalogues").exists() {
            return ancestor.to_path_buf();
        }
    }
    cwd
}
