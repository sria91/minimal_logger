use crate::config::{
    ActiveConfig, DEFAULT_LOG_FORMAT, LogFormat, MAX_BUF_CAPACITY, MAX_FLUSH_MS,
    MAX_FORMAT_FIELD_WIDTH, MAX_FORMAT_TEMPLATE_LEN,
};

use super::*;
use log::{Level, LevelFilter, Record};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_TEMP_LOG: AtomicUsize = AtomicUsize::new(0);

fn temp_log_path(name: &str) -> std::path::PathBuf {
    let id = NEXT_TEMP_LOG.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "minimal_logger_{name}_{}_{}.log",
        std::process::id(),
        id
    ))
}

#[test]
fn default_log_format_renders() {
    let format = LogFormat::parse(DEFAULT_LOG_FORMAT);
    let record = Record::builder()
        .args(format_args!("hello"))
        .level(Level::Info)
        .target("test")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("lib.rs"))
        .line(Some(123))
        .build();

    assert!(
        format
            .render(&record)
            .contains("T[tests::default_log_format_renders]")
    );
    assert!(format.render(&record).contains("[INFO ]"));
    assert!(format.render(&record).ends_with("hello\n"));
}

#[test]
fn custom_log_format_renders_placeholders() {
    let format = LogFormat::parse("{level}|{target}|{args}|{module_path}|{file}|{line}");
    let record = Record::builder()
        .args(format_args!("ok"))
        .level(Level::Debug)
        .target("myapp")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("lib.rs"))
        .line(Some(123))
        .build();

    assert_eq!(
        format.render(&record),
        "DEBUG|myapp|ok|minimal_logger::tests|lib.rs|123\n"
    );
}

#[test]
fn escaped_braces_render_literal_braces() {
    let format = LogFormat::parse("{{level}} {level}");
    let record = Record::builder()
        .args(format_args!("y"))
        .level(Level::Warn)
        .target("x")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("lib.rs"))
        .line(Some(123))
        .build();

    assert_eq!(format.render(&record), "{level} WARN\n");
}
#[test]
fn timestamp_placeholder_renders() {
    let format = LogFormat::parse("{timestamp} {level} {args}");
    let record = Record::builder()
        .args(format_args!("test"))
        .level(Level::Info)
        .target("test")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("lib.rs"))
        .line(Some(123))
        .build();

    let output = format.render(&record);
    // Timestamp must match the ISO-8601 pattern YYYY-MM-DDThh:mm:ss.xxxxxxZ.
    let ts = output.split_whitespace().next().unwrap_or("");
    assert!(
        ts.len() == 27 && ts.ends_with('Z') && ts.contains('T'),
        "timestamp did not match expected ISO-8601 format: got {ts:?}"
    );
    assert!(output.contains("INFO"));
    assert!(output.ends_with('\n'));
}

#[test]
fn thread_name_placeholder_renders() {
    let format = LogFormat::parse("{thread_name} {level} {args}");
    let record = Record::builder()
        .args(format_args!("test"))
        .level(Level::Info)
        .target("test")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("lib.rs"))
        .line(Some(123))
        .build();

    let output = format.render(&record);
    let current_thread = std::thread::current();
    let thread_name = current_thread.name().unwrap_or("unnamed");
    assert!(output.starts_with(thread_name));
    assert!(output.contains("INFO"));
    assert!(output.ends_with('\n'));
}

#[test]
fn level_for_prefers_more_specific_target_filter() {
    let cfg = ActiveConfig::from_reload(
        MinimalLoggerConfig::new()
            .level(LevelFilter::Info)
            .filter("app", LevelFilter::Warn)
            .filter("app::sub", LevelFilter::Debug)
            .into_reload(None),
    );

    assert_eq!(cfg.level_for("app::sub::worker"), LevelFilter::Debug);
    assert_eq!(cfg.level_for("app::other"), LevelFilter::Warn);
    assert_eq!(cfg.level_for("other"), LevelFilter::Info);
}

#[test]
fn enabled_respects_target_filter_levels() {
    let cfg = ActiveConfig::from_reload(
        MinimalLoggerConfig::new()
            .level(LevelFilter::Info)
            .filter("app::sub", LevelFilter::Debug)
            .into_reload(None),
    );

    let record = Record::builder()
        .args(format_args!("ok"))
        .level(Level::Debug)
        .target("app::sub::worker")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("lib.rs"))
        .line(Some(1))
        .build();

    assert!(record.metadata().level() <= cfg.level_for(record.metadata().target()));

    let record = Record::builder()
        .args(format_args!("ok"))
        .level(Level::Debug)
        .target("app::other")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("lib.rs"))
        .line(Some(1))
        .build();

    assert!(record.metadata().level() > cfg.level_for(record.metadata().target()));
}

// ─────────────────────────────────────────────────────────────────────────────
// Format spec: right-alignment and zero-width padding
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn right_alignment_pads_on_left() {
    let format = LogFormat::parse("{line:>4}");
    let record = Record::builder()
        .args(format_args!(""))
        .level(Level::Info)
        .target("t")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("f"))
        .line(Some(7))
        .build();
    // Line "7" right-aligned in width 4 → "   7"
    assert!(format.render(&record).starts_with("   7"));
}

#[test]
fn format_spec_width_zero_means_no_padding() {
    // Width 0 (no colon spec) — value emitted as-is.
    let format = LogFormat::parse("{level}");
    let record = Record::builder()
        .args(format_args!(""))
        .level(Level::Warn)
        .target("t")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("f"))
        .line(Some(1))
        .build();
    assert_eq!(format.render(&record).trim_end(), "WARN");
}

// ─────────────────────────────────────────────────────────────────────────────
// flush_ms = 0: no background thread spawned, records flush synchronously
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn flush_ms_zero_does_not_spawn_background_thread() {
    use crate::logger::FlushWorker;

    let worker = FlushWorker::spawn_for_test(0);
    assert!(
        !worker.has_handle(),
        "flush_ms=0 must not spawn a background thread"
    );
}

#[test]
fn flush_ms_zero_flushes_each_record_after_write() {
    crate::log_file::flush_and_clear_thread_writer();
    let path = temp_log_path("flush_each_record");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open temp log file");

    let log_file = std::sync::Arc::new(crate::log_file::LogFile::new(file, 64));
    let format = LogFormat::parse("{args}");
    let record = Record::builder()
        .args(format_args!("flushed"))
        .level(Level::Info)
        .target("t")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("f"))
        .line(Some(1))
        .build();

    crate::log_file::write_record(&record, std::sync::Arc::clone(&log_file), 64, true, &format);

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "flushed\n");
    crate::log_file::flush_and_clear_thread_writer();
    drop(log_file);
    let _ = std::fs::remove_file(path);
}

#[test]
fn reconfiguring_to_stderr_flushes_current_thread_file_buffer() {
    crate::log_file::flush_and_clear_thread_writer();
    let path = temp_log_path("stderr_reinit");

    let logger = crate::logger::MinimalLogger::from_config(
        MinimalLoggerConfig::new()
            .file(&path)
            .format("{args}")
            .buf_capacity(1024)
            .flush_ms(MAX_FLUSH_MS),
    );

    let record = Record::builder()
        .args(format_args!("before-stderr"))
        .level(Level::Info)
        .target("t")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("f"))
        .line(Some(1))
        .build();

    logger.log(&record);

    let current = logger.state.load();
    let reload = MinimalLoggerConfig::new()
        .stderr()
        .into_reload(Some(&current.reload));
    drop(current);
    logger.apply_reload(reload);

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "before-stderr\n");
    drop(logger);
    let _ = std::fs::remove_file(path);
}

#[test]
fn flush_file_buffers_drain_records_from_other_threads() {
    let path = temp_log_path("cross_thread_flush");
    let logger = std::sync::Arc::new(crate::logger::MinimalLogger::from_config(
        MinimalLoggerConfig::new()
            .file(&path)
            .format("{args}")
            .buf_capacity(1024)
            .flush_ms(MAX_FLUSH_MS),
    ));

    let worker_logger = std::sync::Arc::clone(&logger);
    let worker = std::thread::spawn(move || {
        let record = Record::builder()
            .args(format_args!("from-worker"))
            .level(Level::Info)
            .target("t")
            .module_path_static(Some("minimal_logger::tests"))
            .file_static(Some("f"))
            .line(Some(1))
            .build();
        worker_logger.log(&record);
    });

    worker.join().expect("worker log thread should complete");

    logger.flush_file_buffers();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "from-worker\n");

    drop(logger);
    let _ = std::fs::remove_file(path);
}

#[cfg(unix)]
#[test]
fn created_log_files_do_not_grant_group_or_world_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let path = temp_log_path("permissions");
    let logger = crate::logger::MinimalLogger::from_config(MinimalLoggerConfig::new().file(&path));
    drop(logger);

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode & 0o077, 0, "group/other permission bits must be clear");
    let _ = std::fs::remove_file(path);
}

#[test]
fn oversized_config_values_are_bounded() {
    let cfg = MinimalLoggerConfig::new()
        .buf_capacity(MAX_BUF_CAPACITY + 1)
        .flush_ms(MAX_FLUSH_MS + 1)
        .format("x".repeat(MAX_FORMAT_TEMPLATE_LEN + 1));

    assert_eq!(cfg.get_buf_capacity(), Some(MAX_BUF_CAPACITY));
    assert_eq!(cfg.get_flush_ms(), Some(MAX_FLUSH_MS));
    assert_eq!(cfg.get_format(), Some(DEFAULT_LOG_FORMAT));

    let format = LogFormat::parse(&format!("{{level:<{}}}", MAX_FORMAT_FIELD_WIDTH + 1));
    let record = Record::builder()
        .args(format_args!(""))
        .level(Level::Info)
        .target("t")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("f"))
        .line(Some(1))
        .build();

    let rendered = format.render(&record);
    assert!(rendered.starts_with("INFO"));
    assert_eq!(
        rendered.trim_end_matches('\n').len(),
        MAX_FORMAT_FIELD_WIDTH
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// reinit() delta semantics
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn reinit_unset_fields_inherit_current_value() {
    // Build a ReloadConfig with all fields set.
    let base = MinimalLoggerConfig::new()
        .level(LevelFilter::Debug)
        .filter("app", LevelFilter::Trace)
        .flush_ms(2000)
        .buf_capacity(8192)
        .format("{level} {args}")
        .into_reload(None);

    // A config with only the level changed — all other fields should inherit.
    let delta = MinimalLoggerConfig::new()
        .level(LevelFilter::Warn)
        .into_reload(Some(&base));

    assert_eq!(
        delta.default_level,
        LevelFilter::Warn,
        "level should update"
    );
    assert_eq!(delta.flush_ms, 2000, "flush_ms should inherit");
    assert_eq!(delta.buf_capacity, 8192, "buf_capacity should inherit");
    assert_eq!(
        delta.format_template, "{level} {args}",
        "format should inherit"
    );
    assert_eq!(delta.filters, base.filters, "filters should inherit");
}
