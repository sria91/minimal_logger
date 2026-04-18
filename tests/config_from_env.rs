//! Integration tests for [`minimal_logger::config_from_env()`].
//!
//! Each test sets a specific combination of `RUST_LOG*` environment variables,
//! calls `config_from_env()`, and asserts that the returned
//! [`MinimalLoggerConfig`] was populated correctly.
//!
//! Tests run with `RUST_TEST_THREADS=1` (or sequentially via `cargo test --
//! --test-threads=1`) to prevent env-var mutation from racing between tests.
//! Alternatively, each test uses a unique variable set and restores the
//! previous value on exit, so they are safe to run in parallel.

use log::LevelFilter;
use std::env;
use std::sync::Mutex;

/// Serialises all tests that mutate environment variables so they cannot race.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Helper that saves the previous value of an env var, sets a new one, and
/// restores the old value when the guard is dropped.
struct EnvGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = env::var(key).ok();
        // SAFETY: single-threaded test or test with unique keys.
        unsafe { env::set_var(key, value) };
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = env::var(key).ok();
        unsafe { env::remove_var(key) };
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(v) => unsafe { env::set_var(self.key, v) },
            None => unsafe { env::remove_var(self.key) },
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// RUST_LOG – global level
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn rust_log_sets_global_level_debug() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::set("RUST_LOG", "debug");

    let cfg = minimal_logger::config_from_env();
    assert_eq!(cfg.get_level(), Some(LevelFilter::Debug));
    assert!(cfg.get_filters().is_empty());
}

#[test]
fn rust_log_sets_global_level_warn() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::set("RUST_LOG", "warn");

    let cfg = minimal_logger::config_from_env();
    assert_eq!(cfg.get_level(), Some(LevelFilter::Warn));
}

#[test]
fn rust_log_sets_global_level_error() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::set("RUST_LOG", "error");

    let cfg = minimal_logger::config_from_env();
    assert_eq!(cfg.get_level(), Some(LevelFilter::Error));
}

#[test]
fn rust_log_sets_global_level_trace() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::set("RUST_LOG", "trace");

    let cfg = minimal_logger::config_from_env();
    assert_eq!(cfg.get_level(), Some(LevelFilter::Trace));
}

#[test]
fn rust_log_sets_global_level_off() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::set("RUST_LOG", "off");

    let cfg = minimal_logger::config_from_env();
    assert_eq!(cfg.get_level(), Some(LevelFilter::Off));
}

#[test]
fn rust_log_absent_defaults_to_info() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::remove("RUST_LOG");

    let cfg = minimal_logger::config_from_env();
    // When RUST_LOG is absent the function falls back to "info".
    assert_eq!(cfg.get_level(), Some(LevelFilter::Info));
}

// ─────────────────────────────────────────────────────────────────────────────
// RUST_LOG – per-target filters
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn rust_log_parses_single_target_filter() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::set("RUST_LOG", "myapp=debug");

    let cfg = minimal_logger::config_from_env();
    let filters = cfg.get_filters();
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].0, "myapp");
    assert_eq!(filters[0].1, LevelFilter::Debug);
}

#[test]
fn rust_log_parses_global_level_and_target_filter() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::set("RUST_LOG", "warn,myapp::db=trace");

    let cfg = minimal_logger::config_from_env();
    assert_eq!(cfg.get_level(), Some(LevelFilter::Warn));

    let filters = cfg.get_filters();
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].0, "myapp::db");
    assert_eq!(filters[0].1, LevelFilter::Trace);
}

#[test]
fn rust_log_parses_multiple_target_filters() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::set("RUST_LOG", "info,myapp=warn,myapp::net=debug");

    let cfg = minimal_logger::config_from_env();
    assert_eq!(cfg.get_level(), Some(LevelFilter::Info));

    let filters = cfg.get_filters();
    assert_eq!(filters.len(), 2);

    let map: std::collections::HashMap<&str, LevelFilter> =
        filters.iter().map(|(t, l)| (t.as_str(), *l)).collect();
    assert_eq!(map["myapp"], LevelFilter::Warn);
    assert_eq!(map["myapp::net"], LevelFilter::Debug);
}

// ─────────────────────────────────────────────────────────────────────────────
// RUST_LOG_FILE
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn rust_log_file_sets_file_path() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::set("RUST_LOG_FILE", "/tmp/myapp.log");

    let cfg = minimal_logger::config_from_env();
    assert_eq!(
        cfg.get_file_path(),
        Some(std::path::Path::new("/tmp/myapp.log"))
    );
}

#[test]
fn rust_log_file_absent_leaves_path_none() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::remove("RUST_LOG_FILE");

    let cfg = minimal_logger::config_from_env();
    assert!(cfg.get_file_path().is_none());
}

// ─────────────────────────────────────────────────────────────────────────────
// RUST_LOG_BUFFER_SIZE
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn rust_log_buffer_size_sets_capacity() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::set("RUST_LOG_BUFFER_SIZE", "8192");

    let cfg = minimal_logger::config_from_env();
    assert_eq!(cfg.get_buf_capacity(), Some(8192));
}

#[test]
fn rust_log_buffer_size_absent_leaves_capacity_none() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::remove("RUST_LOG_BUFFER_SIZE");

    let cfg = minimal_logger::config_from_env();
    assert!(cfg.get_buf_capacity().is_none());
}

#[test]
fn rust_log_buffer_size_invalid_leaves_capacity_none() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::set("RUST_LOG_BUFFER_SIZE", "not_a_number");

    let cfg = minimal_logger::config_from_env();
    assert!(cfg.get_buf_capacity().is_none());
}

// ─────────────────────────────────────────────────────────────────────────────
// RUST_LOG_FLUSH_MS
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn rust_log_flush_ms_sets_interval() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::set("RUST_LOG_FLUSH_MS", "500");

    let cfg = minimal_logger::config_from_env();
    assert_eq!(cfg.get_flush_ms(), Some(500));
}

#[test]
fn rust_log_flush_ms_absent_leaves_interval_none() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::remove("RUST_LOG_FLUSH_MS");

    let cfg = minimal_logger::config_from_env();
    assert!(cfg.get_flush_ms().is_none());
}

#[test]
fn rust_log_flush_ms_invalid_leaves_interval_none() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::set("RUST_LOG_FLUSH_MS", "forever");

    let cfg = minimal_logger::config_from_env();
    assert!(cfg.get_flush_ms().is_none());
}

// ─────────────────────────────────────────────────────────────────────────────
// RUST_LOG_FORMAT
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn rust_log_format_sets_template() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::set("RUST_LOG_FORMAT", "{level} {args}");

    let cfg = minimal_logger::config_from_env();
    assert_eq!(cfg.get_format(), Some("{level} {args}"));
}

#[test]
fn rust_log_format_absent_leaves_template_none() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _g = EnvGuard::remove("RUST_LOG_FORMAT");

    let cfg = minimal_logger::config_from_env();
    assert!(cfg.get_format().is_none());
}

// ─────────────────────────────────────────────────────────────────────────────
// All variables set simultaneously
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn all_env_vars_set_populates_all_fields() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _rust_log = EnvGuard::set("RUST_LOG", "debug,myapp=trace");
    let _rust_log_file = EnvGuard::set("RUST_LOG_FILE", "/tmp/all_fields.log");
    let _rust_log_buf = EnvGuard::set("RUST_LOG_BUFFER_SIZE", "16384");
    let _rust_log_flush = EnvGuard::set("RUST_LOG_FLUSH_MS", "250");
    let _rust_log_fmt = EnvGuard::set("RUST_LOG_FORMAT", "{timestamp} {level} {args}");

    let cfg = minimal_logger::config_from_env();

    assert_eq!(cfg.get_level(), Some(LevelFilter::Debug));

    let filters = cfg.get_filters();
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].0, "myapp");
    assert_eq!(filters[0].1, LevelFilter::Trace);

    assert_eq!(
        cfg.get_file_path(),
        Some(std::path::Path::new("/tmp/all_fields.log"))
    );
    assert_eq!(cfg.get_buf_capacity(), Some(16384));
    assert_eq!(cfg.get_flush_ms(), Some(250));
    assert_eq!(cfg.get_format(), Some("{timestamp} {level} {args}"));
}
