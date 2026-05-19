//! Minimal-resource logger for Rust applications.
//!
//! `minimal_logger` provides a low-allocation, thread-buffered logger
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
//! | `.flush_ms(ms)`    | `1000`            | Periodic flush interval; `0` flushes every record   |
//! | `.format(tmpl)`    | *see below*       | Log-line template with `{field}` placeholders       |
//!
//! New Unix log files are created with owner-only permissions, subject to the
//! process umask. Oversized environment/config values are bounded to avoid
//! accidental memory or latency spikes.
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
//! {timestamp} [{level:<5}] T[{thread_name}] [{target}] {args}
//! ```
//!
//! # Lifecycle
//!
//! 1. [`init()`] registers the global logger and starts the flush worker thread
//!    when the configured flush interval is non-zero.
//! 2. Log macros (`info!`, `debug!`, …) render one log line and write it into
//!    a shared buffered file writer (or stderr when file output is disabled).
//! 3. [`reinit()`] applies a new [`MinimalLoggerConfig`] and updates only the subsystems
//!    whose effective configuration changed.
//! 4. [`shutdown()`] flushes active buffered output before exit.
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
//! - **Output file** — destination swapped atomically; the previous writer is
//!   flushed/synced before close.
//! - **Flush interval** — old worker thread stopped and joined; new one spawned.
//! - **Format template** — new parsed template swapped in atomically.
//! - **Buffer size** — applied to newly opened log files.
//!
//! # Flush model
//!
//! | Trigger      | Mechanism                                                       |
//! |--------------|-----------------------------------------------------------------|
//! | Buffer full  | `BufWriter` flushes automatically on the next `write_all` call  |
//! | Periodic     | Background worker flushes the active shared file buffer         |
//! | Explicit     | [`shutdown()`] or `Log::flush()` flushes the active buffer      |
//!
//! # Log rotation
//!
//! | Platform       | Signal / mechanism                        |
//! |----------------|-------------------------------------------|
//! | Linux / macOS  | `SIGHUP`                                  |
//! | Other Unix     | `SIGHUP`                                  |
//! | Windows        | `Local\RustLogger_LogRotate` named event |
//!
//! When rotation fires, the logger opens a new file and atomically swaps the
//! active writer. The old writer is flushed and synced before close.

mod config;
mod log_file;
mod logger;
mod native;

use log::{Log, SetLoggerError};
use std::sync::atomic::{AtomicBool, Ordering};

pub use crate::config::MinimalLoggerConfig;
#[allow(deprecated)]
pub use crate::config::config_from_env;
use crate::{
    logger::{LOGGER, MinimalLogger, install_runtime_hooks_once},
    native::platform,
};

// ─────────────────────────────────────────────────────────────────────────────
// Atomic flags
//
// REOPEN_FLAG : set by platform signal/event → triggers LogFile swap in log().
//
// REOPEN_FLAG is checked in Log::log() and handled by reopening/switching the
// shared active file writer.
// ─────────────────────────────────────────────────────────────────────────────

/// Set by the platform rotation handler (`SIGHUP` / named event) to request
/// a log file reopen on the next `Log::log` call.
static REOPEN_FLAG: AtomicBool = AtomicBool::new(false);
/// Ensures repeated I/O failures do not recursively flood stderr.
static IO_ERROR_REPORTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn report_io_error(context: &str, err: &std::io::Error) {
    if !IO_ERROR_REPORTED.swap(true, Ordering::Relaxed) {
        eprintln!("[minimal_logger] {context} failed: {err}");
    }
}

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
/// - A panic hook that flushes active buffered output before the process unwinds.
pub fn init(config: MinimalLoggerConfig) -> Result<(), SetLoggerError> {
    if let Some(logger) = LOGGER.get().copied() {
        return log::set_logger(logger);
    }

    let raw = Box::into_raw(Box::new(MinimalLogger::from_config(config)));
    // SAFETY: `raw` came from `Box::into_raw` and remains valid until either
    // `log::set_logger` succeeds and the logger intentionally lives for the
    // process lifetime, or the error path reconstructs and drops the box.
    let logger = unsafe { &*raw };

    if let Err(err) = log::set_logger(logger) {
        // SAFETY: `set_logger` failed, so the global log facade did not retain
        // this reference. Rebuild the Box to stop the worker and close the file.
        unsafe { drop(Box::from_raw(raw)) };
        return Err(err);
    }

    let already_set = LOGGER.set(logger).is_err();
    debug_assert!(!already_set);

    install_runtime_hooks_once();

    let max = logger.state.load().max_level;
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
/// | `.file()` / `.stderr()`  | Destination swapped; old writer flushed/synced on switch |
/// | `.flush_ms()`            | Old flush worker stopped and joined; new worker spawned |
/// | `.format()`              | Format template swapped atomically                      |
/// | `.buf_capacity()`        | Applied when a new file writer is opened                |
///
/// If the logger has not yet been initialised this function behaves like
/// [`init()`] with the supplied `config`, discarding any initialisation error.
pub fn reinit(config: MinimalLoggerConfig) {
    match LOGGER.get().copied() {
        None => {
            let _ = init(config);
        }
        Some(logger) => {
            let current = logger.state.load();
            let reload = config.into_reload(Some(&current.reload));
            drop(current);
            logger.apply_reload(reload);
        }
    }
}

/// Flush active buffered output and stop the background flush worker.
///
/// Call this near process exit to ensure all buffered log records are written
/// and the flush worker thread has exited cleanly.
///
/// This function does not close the log file descriptor; the OS releases it
/// when the process exits.
pub fn shutdown() {
    if let Some(logger) = LOGGER.get().copied() {
        logger.flush();
        logger.stop_flush_worker();
    }
}

#[cfg(test)]
mod tests;
