//! Minimal-resource logger for Rust applications.
//!
//! `minimal_logger` provides a zero-allocation-on-hot-path, thread-buffered logger
//! with optional file output, periodic flushing, platform-native log rotation, and
//! change-aware runtime reconfiguration via a builder API.
//!
//! # Quick start
//!
//! ```rust
//! use log::{error, info, warn};
//!
//! fn main() {
//!     minimal_logger::init(minimal_logger::MinimalLoggerConfig::new())
//!         .expect("failed to initialise logger");
//!
//!     info!("application started");
//!     warn!("disk usage above 80%");
//!     error!("connection refused");
//!
//!     minimal_logger::shutdown();
//! }
//! ```
//!
//! # Configuration
//!
//! All settings are passed through a [`MinimalLoggerConfig`] builder. Unset fields use
//! compile-time defaults on [`init()`] and keep their current value on [`reinit()`].
//!
//! | Builder method     | Default at `init` | Description                                         |
//! |--------------------|-------------------|-----------------------------------------------------|
//! | `.level(l)`        | `Info`            | Global log level                                    |
//! | `.filter(t, l)`    | *(none)*          | Per-target level override; may be called many times |
//! | `.file(path)`      | *(stderr)*        | Append log records to a file (`O_APPEND`)           |
//! | `.stderr()`        | *(default)*       | Explicitly route output back to stderr              |
//! | `.buf_capacity(n)` | `4096`            | Per-thread `BufWriter` capacity in bytes            |
//! | `.flush_ms(ms)`    | `1000`            | Periodic flush interval in milliseconds             |
//! | `.format(tmpl)`    | *see below*       | Log-line template with `{field}` placeholders       |
//!
//! ## Reading from environment variables
//!
//! [`config_from_env()`] reads the standard `RUST_LOG*` environment variables and
//! returns a pre-populated [`MinimalLoggerConfig`]. Builder methods can be chained
//! to override individual settings before passing to [`init()`] or [`reinit()`]:
//!
//! ```rust,no_run
//! // Pure environment-variable configuration.
//! minimal_logger::init(minimal_logger::config_from_env())
//!     .expect("logger init failed");
//!
//! // Env vars as a baseline with a programmatic override.
//! minimal_logger::init(
//!     minimal_logger::config_from_env().level(log::LevelFilter::Debug)
//! ).expect("logger init failed");
//! ```
//!
//! ## Level and filter syntax
//!
//! Set a global default level and optional per-target overrides:
//!
//! ```rust,no_run
//! minimal_logger::init(
//!     minimal_logger::MinimalLoggerConfig::new()
//!         .level(log::LevelFilter::Debug)               // all targets at DEBUG
//! ).expect("logger init failed");
//!
//! minimal_logger::init(
//!     minimal_logger::MinimalLoggerConfig::new()
//!         .level(log::LevelFilter::Warn)                // global WARN
//!         .filter("myapp", log::LevelFilter::Debug)     // myapp and submodules at DEBUG
//! ).expect("logger init failed");
//! ```
//!
//! Recognised levels: `Off`, `Error`, `Warn`, `Info`, `Debug`, `Trace`.
//! When multiple filters match a target the most specific (longest prefix) wins.
//!
//! ## Format fields
//!
//! | Placeholder     | Example output                |
//! |-----------------|-------------------------------|
//! | `{timestamp}`   | `2026-04-18T12:34:56.789012Z` |
//! | `{level}`       | `INFO`                        |
//! | `{thread_name}` | `main`                        |
//! | `{target}`      | `myapp::server`               |
//! | `{module_path}` | `myapp::server`               |
//! | `{file}`        | `src/server.rs`               |
//! | `{line}`        | `42`                          |
//! | `{args}`        | `listening on :8080`          |
//!
//! Width and alignment: `{level:<5}` left-aligns in a field of width 5;
//! `{line:>4}` right-aligns. Use `{{` and `}}` for literal braces.
//!
//! Default format string:
//! ```text
//! {timestamp} [{level:<5}] T[{thread_name}] [{file}:{line}] {args}
//! ```
//!
//! # Lifecycle
//!
//! 1. [`init()`] registers the global logger and starts the flush worker thread.
//! 2. Log macros (`info!`, `debug!`, …) write into the calling thread's `BufWriter`
//!    with no heap allocation on the steady-state path.
//! 3. [`reinit()`] applies a new [`MinimalLoggerConfig`] and updates only the subsystems
//!    whose effective configuration changed.
//! 4. [`shutdown()`] flushes the calling thread's buffered writer to the kernel.
//!
//! # Runtime reconfiguration
//!
//! Build a [`MinimalLoggerConfig`] with only the fields you want to change and call
//! [`reinit()`]. Unset fields keep their current values — no need to repeat the
//! full configuration:
//!
//! ```rust,no_run
//! // Switch to debug level without touching file, format, or flush settings.
//! minimal_logger::reinit(
//!     minimal_logger::MinimalLoggerConfig::new().level(log::LevelFilter::Debug)
//! );
//! ```
//!
//! Only the components whose resolved value actually changed are updated:
//!
//! - **Filters / max level** — recomputed and applied via `log::set_max_level`.
//! - **Output file** — old file flushed, synced, and closed; new file opened.
//! - **Flush interval** — old worker thread stopped and joined; new one spawned.
//! - **Format template** — new parsed template swapped in atomically.
//! - **Buffer size** — applied the next time a thread-local writer is recreated.
//!
//! # Flush model
//!
//! | Trigger      | Mechanism                                                       |
//! |--------------|-----------------------------------------------------------------|
//! | Buffer full  | `BufWriter` flushes automatically on the next `write_all` call  |
//! | Periodic     | Background worker sets a flag; next log call calls `bw.flush()` |
//! | Thread exit  | `BufWriter::drop()` calls `flush()` automatically               |
//! | Explicit     | [`shutdown()`] or `Log::flush()` flushes the calling thread     |
//!
//! # Log rotation
//!
//! | Platform       | Signal / mechanism                        |
//! |----------------|-------------------------------------------|
//! | Linux / macOS  | `SIGHUP`                                  |
//! | Other Unix     | `SIGHUP`                                  |
//! | Windows        | `Global\RustLogger_LogRotate` named event |
//!
//! When rotation fires, each thread detects the new file on its next log call via
//! an `Arc` pointer comparison: it flushes buffered bytes to the old file descriptor,
//! then creates a new `BufWriter` pointing to the freshly opened file. The old
//! descriptor is closed once no thread holds a reference to it.

mod config;
mod log_file;
mod logger;
mod native;

use log::{Log, SetLoggerError};
use std::sync::atomic::AtomicBool;

pub use crate::config::{MinimalLoggerConfig, config_from_env};
use crate::{
    logger::{LOGGER, MinimalLogger, install_runtime_hooks_once},
    native::platform,
};

// ─────────────────────────────────────────────────────────────────────────────
// Atomic flags
//
// REOPEN_FLAG : set by platform signal/event → triggers LogFile swap in log().
// FLUSH_FLAG  : set by background thread → triggers bw.flush() in write_record().
//
// REOPEN_FLAG is checked only in Log::log(), not inside write_record().
// Rotation is additionally detected per-thread via Arc::ptr_eq() so threads
// that were mid-write when the swap happened catch up on their next log call.
// ─────────────────────────────────────────────────────────────────────────────

/// Set by the platform rotation handler (`SIGHUP` / named event) to request
/// a log file reopen on the next `Log::log` call.
static REOPEN_FLAG: AtomicBool = AtomicBool::new(false);
/// Set by the background flush worker to request a `bw.flush()` call inside
/// `write_record` before the next record is written.
static FLUSH_FLAG: AtomicBool = AtomicBool::new(false);

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: Public API
// ═════════════════════════════════════════════════════════════════════════════

/// Initialise the logger and start the periodic flush worker.
///
/// Call this once at program startup, before any log macros are used.
/// Returns [`Err(SetLoggerError)`](log::SetLoggerError) if a global logger has
/// already been registered (either by this crate on a previous call, or by
/// another logging crate).
///
/// If called more than once the `config` argument is ignored — the singleton
/// is only constructed on the first successful call. Use [`reinit()`] to
/// update a running logger.
///
/// Pass [`MinimalLoggerConfig::new()`] for fully programmatic configuration, or
/// [`config_from_env()`] to read from the `RUST_LOG*` environment variables.
///
/// The first successful call also installs:
/// - A platform rotation handler (`SIGHUP` on Unix; a named event on Windows).
/// - A panic hook that flushes the calling thread's buffer before the process unwinds.
pub fn init(config: MinimalLoggerConfig) -> Result<(), SetLoggerError> {
    let logger = LOGGER.get_or_init(|| MinimalLogger::from_config(config));

    install_runtime_hooks_once();

    let max = logger.state.load().max_level;

    log::set_logger(logger)?;
    log::set_max_level(max);

    Ok(())
}

/// Apply an updated configuration to the running logger.
///
/// Only subsystems whose effective value changed are updated — the rest are
/// left untouched. If the resolved configuration is identical to the active
/// one, this function returns immediately without modifying any runtime state.
///
/// Unset fields in `config` **keep their current value** and do not reset to
/// defaults. See [`MinimalLoggerConfig`] for the full list of configurable fields.
///
/// | Changed setting          | Effect                                                  |
/// |--------------------------|---------------------------------------------------------|
/// | `.level()` / `.filter()` | Log filters and max level updated atomically            |
/// | `.file()` / `.stderr()`  | Old file flushed, synced, and closed; new file opened   |
/// | `.flush_ms()`            | Old flush worker stopped and joined; new worker spawned |
/// | `.format()`              | Format template swapped atomically                      |
/// | `.buf_capacity()`        | Applied when a thread-local writer is next recreated    |
///
/// If the logger has not yet been initialised this function behaves like
/// [`init()`] with the supplied `config`, discarding any initialisation error.
pub fn reinit(config: MinimalLoggerConfig) {
    if LOGGER.get().is_none() {
        let _ = init(config);
        return;
    }

    if let Some(logger) = LOGGER.get() {
        let current = logger.state.load();
        let reload = config.into_reload(Some(&current.reload));
        drop(current);
        logger.apply_reload(reload);
    }
}

/// Flush the calling thread's buffered writer to the kernel.
///
/// Call this near process exit to ensure all buffered log records are written.
/// Other threads' writers are flushed automatically when those threads exit
/// via `BufWriter::drop()`.
///
/// This function does not stop the background flush worker or close the log
/// file. If the process exits normally the OS will release remaining file
/// descriptors.
pub fn shutdown() {
    if let Some(logger) = LOGGER.get() {
        logger.flush();
    }
}

#[cfg(test)]
mod tests;
