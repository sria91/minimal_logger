use super::*;
use log::{Level, LevelFilter, Record};

#[test]
fn default_log_format_renders() {
    let format = LogFormat::parse(DEFAULT_LOG_FORMAT);
    let record = Record::builder()
        .args(format_args!("{}", "hello"))
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
        .args(format_args!("{}", "ok"))
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
        .args(format_args!("{}", "y"))
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
        .args(format_args!("{}", "test"))
        .level(Level::Info)
        .target("test")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("lib.rs"))
        .line(Some(123))
        .build();

    let output = format.render(&record);
    assert!(output.contains("T"));
    assert!(output.contains("INFO"));
    assert!(output.ends_with('\n'));
}

#[test]
fn thread_name_placeholder_renders() {
    let format = LogFormat::parse("{thread_name} {level} {args}");
    let record = Record::builder()
        .args(format_args!("{}", "test"))
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
    let mut filters = vec![
        TargetFilter {
            target: "app".to_string(),
            level: LevelFilter::Warn,
        },
        TargetFilter {
            target: "app::sub".to_string(),
            level: LevelFilter::Debug,
        },
    ];
    filters.sort_unstable_by(|a, b| b.target.len().cmp(&a.target.len()));

    let logger = MinimalLogger {
        default_level: LevelFilter::Info,
        filters,
        file_path: None,
        buf_capacity: DEFAULT_BUF_CAPACITY,
        flush_ms: DEFAULT_FLUSH_MS,
        format: LogFormat::parse(DEFAULT_LOG_FORMAT),
        file: ArcSwapOption::new(None),
    };

    assert_eq!(logger.level_for("app::sub::worker"), LevelFilter::Debug);
    assert_eq!(logger.level_for("app::other"), LevelFilter::Warn);
    assert_eq!(logger.level_for("other"), LevelFilter::Info);
}

#[test]
fn enabled_respects_target_filter_levels() {
    let logger = MinimalLogger {
        default_level: LevelFilter::Info,
        filters: vec![TargetFilter {
            target: "app::sub".to_string(),
            level: LevelFilter::Debug,
        }],
        file_path: None,
        buf_capacity: DEFAULT_BUF_CAPACITY,
        flush_ms: DEFAULT_FLUSH_MS,
        format: LogFormat::parse(DEFAULT_LOG_FORMAT),
        file: ArcSwapOption::new(None),
    };

    let record = Record::builder()
        .args(format_args!("ok"))
        .level(Level::Debug)
        .target("app::sub::worker")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("lib.rs"))
        .line(Some(1))
        .build();

    assert!(logger.enabled(record.metadata()));

    let record = Record::builder()
        .args(format_args!("ok"))
        .level(Level::Debug)
        .target("app::other")
        .module_path_static(Some("minimal_logger::tests"))
        .file_static(Some("lib.rs"))
        .line(Some(1))
        .build();

    assert!(!logger.enabled(record.metadata()));
}
