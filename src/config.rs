use std::path::{PathBuf, absolute};

use log::{LevelFilter, Record};
use time::OffsetDateTime;

use crate::logger::FileTarget;

/// Default capacity of each thread-local [`BufWriter`], in bytes.
pub(crate) const DEFAULT_BUF_CAPACITY: usize = 4 * 1024;
/// Default interval at which the flush worker wakes and sets `FLUSH_FLAG`, in milliseconds.
pub(crate) const DEFAULT_FLUSH_MS: u64 = 1_000;
/// Default log-line template used when `RUST_LOG_FORMAT` is not set.
pub(crate) const DEFAULT_LOG_FORMAT: &str =
    "{timestamp} [{level:<5}] T[{thread_name}] [{file}:{line}] {args}";

/// A snapshot of resolved logger configuration.
///
/// Produced by [`MinimalLoggerConfig::into_reload`] and compared against the currently
/// active configuration by [`MinimalLogger::apply_reload`]. Equal snapshots
/// mean no reconfiguration is required.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ReloadConfig {
    /// Fallback level used when no per-target filter matches.
    pub(crate) default_level: LevelFilter,
    /// Per-target overrides, sorted by decreasing `target` length for prefix matching.
    pub(crate) filters: Vec<TargetFilter>,
    /// Absolute path to the log file, or `None` for stderr mode.
    pub(crate) file_path: Option<String>,
    /// Desired capacity of each new thread-local `BufWriter`, in bytes.
    pub(crate) buf_capacity: usize,
    /// Flush worker sleep interval, in milliseconds.
    pub(crate) flush_ms: u64,
    /// Raw `RUST_LOG_FORMAT` string kept for equality comparison and re-parsing.
    pub(crate) format_template: String,
}

/// Live runtime configuration derived from a [`ReloadConfig`] snapshot.
///
/// Stored behind an `Arc` inside an [`ArcSwap`] so that [`reinit()`] can swap
/// it atomically while concurrent log calls read it without any locking.
#[derive(Clone)]
pub(crate) struct ActiveConfig {
    /// The environment snapshot this config was compiled from.
    pub(crate) reload: ReloadConfig,
    /// Compiled format template used to render each log record.
    pub(crate) format: LogFormat,
    /// Pre-computed maximum level across all filters; passed to `log::set_max_level`.
    pub(crate) max_level: LevelFilter,
}

impl ActiveConfig {
    /// Build an `ActiveConfig` from a freshly parsed [`ReloadConfig`].
    ///
    /// Compiles the format template and computes the global maximum level.
    pub(crate) fn from_reload(reload: ReloadConfig) -> Self {
        let max_level = reload
            .filters
            .iter()
            .map(|f| f.level)
            .fold(reload.default_level, |acc, level| acc.max(level));

        ActiveConfig {
            format: LogFormat::parse(&reload.format_template),
            reload,
            max_level,
        }
    }

    /// Return the effective [`LevelFilter`] for the given log target.
    ///
    /// Finds the most-specific matching filter (longest target prefix) or falls
    /// back to `default_level` when no filter matches.
    #[inline]
    pub(crate) fn level_for(&self, target: &str) -> LevelFilter {
        self.reload
            .filters
            .iter()
            .find(|f| target.starts_with(f.target.as_str()))
            .map(|f| f.level)
            .unwrap_or(self.reload.default_level)
    }
}

/// Builder for logger configuration.
///
/// Construct with [`MinimalLoggerConfig::new()`] (or [`Default::default()`]) for fully
/// programmatic configuration, or with [`config_from_env()`] to seed the config from
/// the standard `RUST_LOG*` environment variables. Chain builder methods to set or
/// override individual settings, then pass the result to [`init()`] or [`reinit()`].
///
/// # Unset fields
///
/// On [`init()`], any unset field falls back to its compile-time default:
/// `Info` level, stderr output, 4 KiB buffer, 1 s flush interval, and the
/// built-in timestamp/level/thread/file/line format.
///
/// On [`reinit()`], any unset field **keeps its current value** — so you can
/// update a single subsystem without touching the others:
///
/// ```rust,no_run
/// minimal_logger::reinit(
///     minimal_logger::MinimalLoggerConfig::new().level(log::LevelFilter::Debug)
/// );
/// ```
///
/// # Example
///
/// ```rust,no_run
/// minimal_logger::init(
///     minimal_logger::MinimalLoggerConfig::new()
///         .level(log::LevelFilter::Info)
///         .filter("myapp::db", log::LevelFilter::Debug)
///         .format("{timestamp} [{level}] {args}")
/// ).expect("logger init failed");
/// ```
pub struct MinimalLoggerConfig {
    pub(crate) level: Option<LevelFilter>,
    pub(crate) filters: Option<Vec<(String, LevelFilter)>>,
    pub(crate) file: Option<FileTarget>,
    pub(crate) buf_capacity: Option<usize>,
    pub(crate) flush_ms: Option<u64>,
    pub(crate) format: Option<String>,
}

impl Default for MinimalLoggerConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl MinimalLoggerConfig {
    /// Create a new `MinimalLoggerConfig` with all fields unset.
    ///
    /// See the struct documentation for how unset fields are resolved inside
    /// [`init()`] and [`reinit()`].
    pub fn new() -> Self {
        MinimalLoggerConfig {
            level: None,
            filters: None,
            file: None,
            buf_capacity: None,
            flush_ms: None,
            format: None,
        }
    }

    /// Set the global default log level.
    ///
    /// Records whose target does not match any per-target filter (added via
    /// [`filter`](Self::filter)) are emitted at this level or above.
    pub fn level(mut self, level: LevelFilter) -> Self {
        self.level = Some(level);
        self
    }

    /// Add a per-target level override.
    ///
    /// `target` is matched as a prefix of the log record's target string
    /// (typically the module path). The most specific (longest) matching
    /// prefix wins. May be called multiple times.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// minimal_logger::init(
    ///     minimal_logger::MinimalLoggerConfig::new()
    ///         .level(log::LevelFilter::Warn)                 // global default
    ///         .filter("myapp", log::LevelFilter::Info)       // myapp and submodules
    ///         .filter("myapp::db", log::LevelFilter::Trace)  // db at trace
    /// ).expect("logger init failed");
    /// ```
    pub fn filter(mut self, target: impl Into<String>, level: LevelFilter) -> Self {
        self.filters
            .get_or_insert_with(Vec::new)
            .push((target.into(), level));
        self
    }

    /// Write log output to a file at `path` (created with `O_APPEND` if absent).
    ///
    /// The path is resolved to an absolute path when the configuration is applied
    /// (inside [`init()`] or [`reinit()`]). If the file cannot be opened, a
    /// diagnostic is printed to stderr and output falls back to stderr.
    pub fn file(mut self, path: impl Into<PathBuf>) -> Self {
        self.file = Some(FileTarget::Path(path.into()));
        self
    }

    /// Write log output to standard error (the default).
    ///
    /// Use this on [`reinit()`] to switch back from a log file to stderr.
    pub fn stderr(mut self) -> Self {
        self.file = Some(FileTarget::Stderr);
        self
    }

    /// Set the per-thread [`BufWriter`] capacity in bytes (default: 4096).
    ///
    /// The new capacity takes effect the next time a thread's writer is
    /// recreated (on first use or after a log-file rotation).
    pub fn buf_capacity(mut self, bytes: usize) -> Self {
        self.buf_capacity = Some(bytes);
        self
    }

    /// Set the periodic flush interval in milliseconds (default: 1000).
    ///
    /// A background thread wakes every `ms` milliseconds and sets a flag that
    /// causes the next log call on any thread to flush its buffer. Set to `0`
    /// to flush on every log record.
    pub fn flush_ms(mut self, ms: u64) -> Self {
        self.flush_ms = Some(ms);
        self
    }

    /// Set the log-line format template.
    ///
    /// The template is a string with `{field}` placeholders. Supported fields:
    /// `timestamp`, `level`, `thread_name`, `target`, `module_path`, `file`,
    /// `line`, `args`. Width/alignment follows `{level:<5}` syntax. Use `{{`
    /// and `}}` for literal brace characters.
    ///
    /// Default: `{timestamp} [{level:<5}] T[{thread_name}] [{file}:{line}] {args}`
    pub fn format(mut self, template: impl Into<String>) -> Self {
        self.format = Some(template.into());
        self
    }

    /// Return the configured global log level, if set.
    pub fn get_level(&self) -> Option<LevelFilter> {
        self.level
    }

    /// Return the configured per-target filters as a slice of `(target, level)` pairs.
    pub fn get_filters(&self) -> &[(String, LevelFilter)] {
        self.filters.as_deref().unwrap_or(&[])
    }

    /// Return the configured log file path, if set.
    pub fn get_file_path(&self) -> Option<&std::path::Path> {
        match &self.file {
            Some(FileTarget::Path(p)) => Some(p.as_path()),
            _ => None,
        }
    }

    /// Return the configured per-thread buffer capacity in bytes, if set.
    pub fn get_buf_capacity(&self) -> Option<usize> {
        self.buf_capacity
    }

    /// Return the configured flush interval in milliseconds, if set.
    pub fn get_flush_ms(&self) -> Option<u64> {
        self.flush_ms
    }

    /// Return the configured log-line format template string, if set.
    pub fn get_format(&self) -> Option<&str> {
        self.format.as_deref()
    }

    /// Convert this builder into a [`ReloadConfig`], merging with `current`.
    ///
    /// Each unset field inherits from `current` when `Some`, or falls back to
    /// the compile-time default when `None` (the case during [`init()`]).
    pub(crate) fn into_reload(self, current: Option<&ReloadConfig>) -> ReloadConfig {
        let default_level = self
            .level
            .unwrap_or_else(|| current.map_or(LevelFilter::Info, |c| c.default_level));

        let filters = match self.filters {
            Some(vec) => {
                let mut tf: Vec<TargetFilter> = vec
                    .into_iter()
                    .map(|(target, level)| TargetFilter { target, level })
                    .collect();
                tf.sort_unstable_by(|a, b| b.target.len().cmp(&a.target.len()));
                tf
            }
            None => current.map_or_else(Vec::new, |c| c.filters.clone()),
        };

        let file_path = match self.file {
            Some(FileTarget::Path(p)) => Some(absolute(&p).unwrap_or(p).display().to_string()),
            Some(FileTarget::Stderr) => None,
            None => current.and_then(|c| c.file_path.clone()),
        };

        let buf_capacity = self
            .buf_capacity
            .unwrap_or_else(|| current.map_or(DEFAULT_BUF_CAPACITY, |c| c.buf_capacity));

        let flush_ms = self
            .flush_ms
            .unwrap_or_else(|| current.map_or(DEFAULT_FLUSH_MS, |c| c.flush_ms));

        let format_template = self.format.unwrap_or_else(|| {
            current.map_or_else(
                || DEFAULT_LOG_FORMAT.to_string(),
                |c| c.format_template.clone(),
            )
        });

        ReloadConfig {
            default_level,
            filters,
            file_path,
            buf_capacity,
            flush_ms,
            format_template,
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: Environment-variable configuration reader
// ═════════════════════════════════════════════════════════════════════════════

/// Build a [`MinimalLoggerConfig`] from the standard `RUST_LOG*` environment variables.
///
/// This is the only public API surface that reads environment variables. The
/// returned config can be inspected or further modified with builder methods before
/// being passed to [`init()`] or [`reinit()`].
///
/// | Variable               | Description                                          |
/// |------------------------|------------------------------------------------------|
/// | `RUST_LOG`             | Global level and optional `target=level` overrides   |
/// | `RUST_LOG_FILE`        | Path to the log file (omit for stderr output)        |
/// | `RUST_LOG_BUFFER_SIZE` | Per-thread `BufWriter` capacity in bytes             |
/// | `RUST_LOG_FLUSH_MS`    | Periodic flush interval in milliseconds              |
/// | `RUST_LOG_FORMAT`      | Log-line template with `{field}` placeholders        |
///
/// When `RUST_LOG` is unset it defaults to `info`. All other variables, when absent
/// or unparseable, leave the corresponding builder field as `None`: on [`init()`]
/// that resolves to the compile-time default (4096 B buffer, 1 s flush, built-in
/// format, stderr output); on [`reinit()`] it preserves the currently active value.
/// Invalid `RUST_LOG` directives are skipped with a warning on stderr.
///
/// # Example
///
/// ```rust,no_run
/// // Read env vars, then override the level programmatically before init.
/// let config = minimal_logger::config_from_env()
///     .level(log::LevelFilter::Debug);
/// minimal_logger::init(config).expect("logger init failed");
/// ```
pub fn config_from_env() -> MinimalLoggerConfig {
    let rust_log = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    let file = std::env::var("RUST_LOG_FILE")
        .ok()
        .map(|path| FileTarget::Path(PathBuf::from(path)));

    let buf_capacity = std::env::var("RUST_LOG_BUFFER_SIZE")
        .ok()
        .and_then(|s| s.parse().ok());

    let flush_ms = std::env::var("RUST_LOG_FLUSH_MS")
        .ok()
        .and_then(|s| s.parse().ok());

    let format = std::env::var("RUST_LOG_FORMAT").ok();

    let mut level: Option<LevelFilter> = None;
    let mut filters: Vec<(String, LevelFilter)> = Vec::new();

    for directive in rust_log.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match directive.split_once('=') {
            Some((target, level_str)) => match level_str.trim().parse::<LevelFilter>() {
                Ok(l) => filters.push((target.trim().to_string(), l)),
                Err(_) => eprintln!(
                    "[minimal_logger] RUST_LOG: unknown level {:?} — skipping",
                    level_str
                ),
            },
            None => match directive.parse::<LevelFilter>() {
                Ok(l) => level = Some(l),
                Err(_) => eprintln!(
                    "[minimal_logger] RUST_LOG: unknown directive {:?} — skipping",
                    directive
                ),
            },
        }
    }

    MinimalLoggerConfig {
        level,
        filters: if filters.is_empty() {
            None
        } else {
            Some(filters)
        },
        file,
        buf_capacity,
        flush_ms,
        format,
    }
}

/// A single `target=level` directive parsed from the `RUST_LOG` environment variable.
///
/// Directives are sorted by decreasing `target` length before matching so that
/// the most specific prefix always wins.
pub(crate) struct TargetFilter {
    /// Module or crate path prefix matched against `record.target()`.
    target: String,
    /// Maximum level to emit for records whose target starts with `self.target`.
    level: LevelFilter,
}

impl Clone for TargetFilter {
    fn clone(&self) -> Self {
        Self {
            target: self.target.clone(),
            level: self.level,
        }
    }
}

impl PartialEq for TargetFilter {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target && self.level == other.level
    }
}

impl Eq for TargetFilter {}

/// Horizontal alignment direction for a fixed-width format field.
#[derive(Clone, Copy)]
enum Align {
    /// Pad on the right; the value appears at the left edge of the field.
    Left,
    /// Pad on the left; the value appears at the right edge of the field.
    Right,
}

/// Width and alignment parsed from the `:spec` portion of a `{field:spec}` placeholder.
#[derive(Clone, Copy)]
struct FormatSpec {
    /// Direction of space padding.
    align: Align,
    /// Minimum rendered width in characters; `None` means no padding.
    width: Option<usize>,
}

/// A named field that can appear as a `{field}` placeholder in `RUST_LOG_FORMAT`.
#[derive(Clone, Copy)]
enum LogField {
    /// UTC timestamp with microsecond precision (`2026-04-18T12:34:56.789012Z`).
    Timestamp,
    /// Name of the current thread, or `"unnamed"` if none was set.
    ThreadName,
    /// Log level string (`ERROR`, `WARN`, `INFO`, `DEBUG`, `TRACE`).
    Level,
    /// The `log::Record` target, typically the module path of the call site.
    Target,
    /// The formatted log message; `{message}` is an accepted synonym.
    Args,
    /// Rust module path of the call site.
    ModulePath,
    /// Source file name of the call site.
    File,
    /// Source line number of the call site.
    Line,
}

/// One element of a compiled `RUST_LOG_FORMAT` template.
#[derive(Clone)]
enum FormatPiece {
    /// Verbatim text copied directly to the output without substitution.
    Literal(String),
    /// A `{field}` or `{field:spec}` placeholder rendered at log call time.
    Placeholder { field: LogField, spec: FormatSpec },
}

/// A compiled log-line format template.
///
/// Built once from a `RUST_LOG_FORMAT` string via [`LogFormat::parse`] and
/// cloned into each new [`ActiveConfig`]. Rendering on the hot path allocates
/// only the final output `String`.
#[derive(Clone)]
pub(crate) struct LogFormat {
    /// Ordered sequence of literal segments and field placeholders.
    pieces: Vec<FormatPiece>,
}

impl LogFormat {
    /// Compile a `RUST_LOG_FORMAT` template string into a [`LogFormat`].
    ///
    /// `{field}` and `{field:spec}` sequences become [`FormatPiece::Placeholder`]
    /// entries; `{{` and `}}` are unescaped to literal brace characters.
    /// Unrecognised field names are kept as literal `{name}` text.
    pub(crate) fn parse(format: &str) -> Self {
        let mut pieces = Vec::new();
        let mut literal = String::new();
        let mut chars = format.chars().peekable();

        while let Some(ch) = chars.next() {
            match ch {
                '{' => {
                    if chars.peek() == Some(&'{') {
                        chars.next();
                        literal.push('{');
                        continue;
                    }

                    if !literal.is_empty() {
                        pieces.push(FormatPiece::Literal(std::mem::take(&mut literal)));
                    }

                    let mut token = String::new();
                    while let Some(next) = chars.next() {
                        if next == '}' {
                            break;
                        }
                        token.push(next);
                    }

                    let piece = if token.is_empty() {
                        FormatPiece::Literal("{}".to_string())
                    } else {
                        parse_placeholder(&token)
                    };
                    pieces.push(piece);
                }
                '}' => {
                    if chars.peek() == Some(&'}') {
                        chars.next();
                        literal.push('}');
                    } else {
                        literal.push('}');
                    }
                }
                other => literal.push(other),
            }
        }

        if !literal.is_empty() {
            pieces.push(FormatPiece::Literal(literal));
        }

        LogFormat { pieces }
    }

    /// Render a log [`Record`] against this template into a complete log line.
    ///
    /// Appends a trailing `'\n'` if the rendered string does not already end
    /// with one.
    pub(crate) fn render(&self, record: &Record) -> String {
        let mut output = String::new();

        for piece in &self.pieces {
            match piece {
                FormatPiece::Literal(text) => output.push_str(text),
                FormatPiece::Placeholder { field, spec } => {
                    let raw = render_field(*field, record);
                    output.push_str(&apply_format_spec(&raw, *spec));
                }
            }
        }

        if !output.ends_with('\n') {
            output.push('\n');
        }

        output
    }
}

/// Parse the text between `{` and `}` into a [`FormatPiece`].
///
/// Splits on `:` to separate the field name from an optional format spec.
/// Returns a [`FormatPiece::Literal`] if the field name is not recognised.
fn parse_placeholder(token: &str) -> FormatPiece {
    let (name, spec_text) = token.split_once(':').unwrap_or((token, ""));
    let spec = parse_format_spec(spec_text);

    let field = match name {
        "timestamp" => LogField::Timestamp,
        "thread_name" => LogField::ThreadName,
        "level" => LogField::Level,
        "target" => LogField::Target,
        "args" | "message" => LogField::Args,
        "module_path" => LogField::ModulePath,
        "file" => LogField::File,
        "line" => LogField::Line,
        _ => {
            return FormatPiece::Literal(format!("{{{}}}", token));
        }
    };

    FormatPiece::Placeholder { field, spec }
}

/// Parse the spec portion of a `{field:spec}` placeholder into a [`FormatSpec`].
///
/// Accepts `<N` (left-align, width N) and `>N` (right-align, width N).
/// Returns a zero-width, left-aligned spec for unrecognised or empty input.
fn parse_format_spec(spec: &str) -> FormatSpec {
    if let Some(width_text) = spec.strip_prefix('<') {
        if let Ok(width) = width_text.parse::<usize>() {
            return FormatSpec {
                align: Align::Left,
                width: Some(width),
            };
        }
    }

    if let Some(width_text) = spec.strip_prefix('>') {
        if let Ok(width) = width_text.parse::<usize>() {
            return FormatSpec {
                align: Align::Right,
                width: Some(width),
            };
        }
    }

    FormatSpec {
        align: Align::Left,
        width: None,
    }
}

/// Render a single [`LogField`] from `record` to a heap-allocated string.
fn render_field(field: LogField, record: &Record) -> String {
    match field {
        LogField::Timestamp => {
            let now = OffsetDateTime::now_utc();
            // Use fixed 6-digit microsecond precision for consistent timestamp length
            now.format(
                &time::format_description::parse(
                    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]Z",
                )
                .unwrap(),
            )
            .unwrap_or_else(|_| "unknown-time".to_string())
        }
        LogField::ThreadName => std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string(),
        LogField::Level => record.level().to_string(),
        LogField::Target => record.target().to_string(),
        LogField::Args => format!("{}", record.args()),
        LogField::ModulePath => record.module_path().unwrap_or_default().to_string(),
        LogField::File => record.file().unwrap_or_default().to_string(),
        LogField::Line => record
            .line()
            .map(|line| line.to_string())
            .unwrap_or_default(),
    }
}

/// Apply a [`FormatSpec`] to a rendered field value, padding with spaces as needed.
///
/// Returns `value` unchanged when `spec.width` is `None` or the string is
/// already at least as wide as the requested minimum field width.
fn apply_format_spec(value: &str, spec: FormatSpec) -> String {
    match spec.width {
        Some(width) if value.len() < width => {
            let pad = width - value.len();
            match spec.align {
                Align::Left => {
                    let mut result = String::with_capacity(width);
                    result.push_str(value);
                    result.extend(std::iter::repeat(' ').take(pad));
                    result
                }
                Align::Right => {
                    let mut result = String::with_capacity(width);
                    result.extend(std::iter::repeat(' ').take(pad));
                    result.push_str(value);
                    result
                }
            }
        }
        _ => value.to_string(),
    }
}
