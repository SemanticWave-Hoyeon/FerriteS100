//! `tracing` setup: console + rotating file appender.
//!
//! The default filter intentionally pins a few noisy targets:
//! - `wgpu_hal::vulkan::conv` is downgraded to `error` because some Vulkan
//!   drivers emit a benign "Unrecognized present mode" warning at every
//!   adapter probe, which otherwise drowns the startup log.

use std::path::Path;

use anyhow::{Context, Result};
use tracing::info;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Initialise console + file logging. `log_path` is a directory; the file
/// `ferrite_debug.log` is created inside it. Honours `RUST_LOG` if set,
/// otherwise applies a sensible per-target default.
pub fn init_logging(log_path: &Path) -> Result<()> {
    std::fs::create_dir_all(log_path)
        .with_context(|| format!("Failed to create log directory: {}", log_path.display()))?;

    let file_appender = RollingFileAppender::new(Rotation::NEVER, log_path, "ferrite_debug.log");

    let console_layer = fmt::layer().with_target(false).with_level(true);
    let file_layer = fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_ansi(false)
        .with_writer(file_appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,ferrite_s100=debug,ferrite_s100_core=debug,ferrite_plugin_loader=debug,ferrite_wgpu=debug,wgpu_hal::vulkan::conv=error",
        )
    });

    tracing_subscriber::registry()
        .with(filter)
        .with(console_layer)
        .with(file_layer)
        .init();

    info!(
        "Logging initialized: {}/ferrite_debug.log",
        log_path.display()
    );

    Ok(())
}
