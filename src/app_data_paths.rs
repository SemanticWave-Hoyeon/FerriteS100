//! Application data locations. Tests and hidden native audits never persist
//! to the real application folder.
use std::path::PathBuf;

pub(crate) fn default_history_path() -> PathBuf {
    if let Some(path) = std::env::var_os("FERRITE_S101_HISTORY_PATH") {
        return PathBuf::from(path);
    }
    if cfg!(test) || ferrite_ui::background_test::enabled() {
        return std::env::temp_dir()
            .join(format!("ferrite-s101-history-{}", std::process::id()))
            .join("history.json");
    }
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base =
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")));
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("FerriteS100/s101-cancellations.json")
}
