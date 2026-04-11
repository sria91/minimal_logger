//! Minimal-resource logger for Rust applications.
//!
//! `minimal_logger` provides a small, thread-buffered logger with
//! optional file output, auto flush, and platform-aware log rotation.
//!
//! # Example
//!
//! ```rust
//! use log::{error, info};
//!
//! fn main() {
//!     minimal_logger::init().expect("failed to initialise logger");
//!     info!("application started");
//!     error!("shutdown due to error");
//!     minimal_logger::shutdown();
//! }
//! ```
//!
//! # Rotation detection
//!
//! Each thread-local `BufWriter<FileWriter>` holds an `Arc<LogFile>` inside
//! `FileWriter`. On every log call, `load_full()` returns the current `Arc<LogFile>`.
//! If `Arc::ptr_eq` fails, a rotation has occurred:
//!   1. Flush old `BufWriter` → drains remaining bytes to old file.
//!   2. Replace with new `BufWriter::with_capacity(capacity, FileWriter(new_arc))`.
//! No separate reopen flag check is needed inside `write_record`.
//!
//! # Flush model
//!
//! | Trigger          | Mechanism                                           |
//! |------------------|-----------------------------------------------------|
//! | Buffer full      | `BufWriter` flushes automatically when internal     |
//! |                  | buffer capacity is reached                          |
//! | Periodic         | Background thread sets `FLUSH_FLAG`; next log call  |
//! |                  | calls `bw.flush()`                                  |
//! | Thread exit      | `BufWriter::drop()` calls `flush()` automatically   |
//! | Explicit         | `shutdown()` / `Log::flush()` calls `bw.flush()`    |
//!
//! # Environment variables
//!
//! | Variable               | Default | Description                           |
//! |------------------------|---------|---------------------------------------|
//! | `RUST_LOG`             | `info`  | Level + per-target filters            |
//! | `RUST_LOG_FILE`        | `stderr`  | Absolute path to log file             |
//! | `RUST_LOG_BUFFER_SIZE` | `4096`  | Per-thread `BufWriter` capacity       |
//! | `RUST_LOG_FLUSH_MS`    | `1000`  | Periodic flush interval (ms)          |
//! | `RUST_LOG_FORMAT`      | `"{timestamp} [{level:<5}] T[{thread_name}] [{file}:{line}] {args}"` | Log message format template (timestamp is fixed 6-digit microseconds) |
//!
//! Supported format fields: `timestamp`, `level`, `thread_name`, `target`,
//! `module_path`, `file`, `line`, `args`.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{PathBuf, absolute};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use log::{LevelFilter, Log, Metadata, Record, SetLoggerError};
use time::OffsetDateTime;

const DEFAULT_BUF_CAPACITY: usize = 4 * 1024;
const DEFAULT_FLUSH_MS: u64 = 1_000;
const DEFAULT_LOG_FORMAT: &str = "{timestamp} [{level:<5}] T[{thread_name}] [{file}:{line}] {args}";

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

static REOPEN_FLAG: AtomicBool = AtomicBool::new(false);
static FLUSH_FLAG: AtomicBool = AtomicBool::new(false);

// ═════════════════════════════════════════════════════════════════════════════
//  PLATFORM: LINUX
// ═════════════════════════════════════════════════════════════════════════════
#[cfg(target_os = "linux")]
mod platform {
    use super::REOPEN_FLAG;
    use std::sync::atomic::Ordering;

    extern "C" fn sighup_handler(_: libc::c_int) {
        REOPEN_FLAG.store(true, Ordering::Relaxed);
    }

    pub fn register_rotation_handler() {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = sighup_handler as *const () as libc::sighandler_t;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut()) != 0 {
                eprintln!("[minimal_logger] sigaction(SIGHUP) failed — rotation disabled");
            }
        }
    }

    /// fdatasync() — data to block device, no metadata update.
    pub fn sync_file(file: &std::fs::File) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::fdatasync(file.as_raw_fd());
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  PLATFORM: macOS
//
//  F_FULLFSYNC required — plain fsync() only reaches the controller cache.
// ═════════════════════════════════════════════════════════════════════════════
#[cfg(target_os = "macos")]
mod platform {
    use super::REOPEN_FLAG;
    use std::sync::atomic::Ordering;

    extern "C" fn sighup_handler(_: libc::c_int) {
        REOPEN_FLAG.store(true, Ordering::Relaxed);
    }

    pub fn register_rotation_handler() {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = sighup_handler as *const () as libc::sighandler_t;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut()) != 0 {
                eprintln!("[minimal_logger] sigaction(SIGHUP) failed — rotation disabled");
            }
        }
    }

    pub fn sync_file(file: &std::fs::File) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC);
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  PLATFORM: WINDOWS
// ═════════════════════════════════════════════════════════════════════════════
#[cfg(windows)]
mod platform {
    use super::REOPEN_FLAG;
    use std::sync::atomic::Ordering;
    use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::FlushFileBuffers;
    use windows::Win32::System::Threading::{
        CreateEventW, INFINITE, ResetEvent, WaitForSingleObject,
    };
    use windows::core::PCWSTR;

    fn to_wide_null(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    pub fn register_rotation_handler() {
        let name = to_wide_null("Global\\RustLogger_LogRotate\0");
        let _ = std::thread::Builder::new()
            .name("logger-rotation-watcher".into())
            .stack_size(64 * 1024)
            .spawn(move || {
                let event =
                    unsafe { CreateEventW(None, true, false, PCWSTR::from_raw(name.as_ptr())) }
                        .unwrap_or(INVALID_HANDLE_VALUE);
                if event.is_invalid() {
                    eprintln!("[minimal_logger] CreateEventW failed — rotation disabled");
                    return;
                }
                loop {
                    unsafe { WaitForSingleObject(event, INFINITE) };
                    REOPEN_FLAG.store(true, Ordering::Relaxed);
                    let _ = unsafe { ResetEvent(event) };
                }
            });
    }

    pub fn sync_file(file: &std::fs::File) {
        use std::os::windows::io::AsRawHandle;
        let _ = unsafe { FlushFileBuffers(HANDLE(file.as_raw_handle())) };
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Fallback: other Unix-like platforms
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(all(unix, not(target_os = "linux"), not(target_os = "macos")))]
mod platform {
    use super::REOPEN_FLAG;
    use std::sync::atomic::Ordering;

    extern "C" fn sighup_handler(_: libc::c_int) {
        REOPEN_FLAG.store(true, Ordering::Relaxed);
    }

    pub fn register_rotation_handler() {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = sighup_handler as libc::sighandler_t;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut());
        }
    }

    pub fn sync_file(file: &std::fs::File) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::fsync(file.as_raw_fd());
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: Target filter
// ═════════════════════════════════════════════════════════════════════════════

struct TargetFilter {
    target: String,
    level: LevelFilter,
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

#[derive(Clone, Copy)]
struct FormatSpec {
    align: Align,
    width: Option<usize>,
}

#[derive(Clone, Copy)]
enum LogField {
    Timestamp,
    ThreadName,
    Level,
    Target,
    Args,
    ModulePath,
    File,
    Line,
}

#[derive(Clone)]
enum FormatPiece {
    Literal(String),
    Placeholder { field: LogField, spec: FormatSpec },
}

#[derive(Clone)]
pub(crate) struct LogFormat {
    pieces: Vec<FormatPiece>,
}

impl LogFormat {
    fn parse(format: &str) -> Self {
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

    fn render(&self, record: &Record) -> String {
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

// ═════════════════════════════════════════════════════════════════════════════
//  UNIX: LogFile — bare File, no Mutex
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(unix)]
pub(crate) struct LogFile {
    pub(crate) file: File,
    #[allow(dead_code)]
    pub(crate) buf_capacity: usize,
}

#[cfg(unix)]
impl LogFile {
    fn new(file: File, buf_capacity: usize) -> Self {
        LogFile { file, buf_capacity }
    }

    /// Flush calling thread's BufWriter then sync file to physical media.
    /// Called from reopen() after Arc::try_unwrap succeeds.
    fn flush_and_sync(&self) {
        with_thread_writer(|bw| {
            let _ = bw.flush();
        });
        platform::sync_file(&self.file);
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  WINDOWS: LogFile — Mutex<File>
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(windows)]
use std::sync::Mutex;

#[cfg(windows)]
pub(crate) struct LogFile {
    pub(crate) file: Mutex<File>,
    #[allow(dead_code)]
    pub(crate) buf_capacity: usize,
}

#[cfg(windows)]
impl LogFile {
    fn new(file: File, buf_capacity: usize) -> Self {
        LogFile {
            file: Mutex::new(file),
            buf_capacity,
        }
    }

    fn flush_and_sync(&self) {
        with_thread_writer(|bw| {
            let _ = bw.flush();
        });
        if let Ok(f) = self.file.lock() {
            platform::sync_file(&*f);
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON + PLATFORM: FileWriter — the W in BufWriter<W>
//
//  This is the only platform-diverging type visible to the BufWriter.
//  BufWriter calls FileWriter::write() when its internal buffer is full
//  or when flush() is called.  BufWriter calls FileWriter::flush() after
//  the data write — our impl is a no-op because the data is already in
//  the kernel page cache after write().
//
//  UNIX path (no lock):
//    write() → (&self.0.file).write(buf)
//    O_APPEND guarantees this write() syscall is atomic — no two threads'
//    bytes interleave regardless of concurrency.
//
//  WINDOWS path (Mutex held only for the syscall):
//    write() → Mutex<File>::lock() → file.write(buf) → unlock
//    The lock is held only for the duration of WriteFile().
//    All formatting happened inside BufWriter's internal buffer before
//    write() was even called — formatting is always lock-free.
//
//  Why BufWriter::flush() calling FileWriter::flush() is a no-op:
//    BufWriter::flush() first calls inner.write(buffered_bytes) to drain
//    its buffer, then calls inner.flush() to propagate the flush.
//    Our write() already delivered the bytes to the kernel (Unix) or to
//    the file (Windows via WriteFile).  There is nothing left to flush at
//    the FileWriter level.  The kernel's own writeback handles disk I/O.
// ═════════════════════════════════════════════════════════════════════════════

pub(crate) struct FileWriter(pub(crate) Arc<LogFile>);

#[cfg(unix)]
impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // &File on Unix implements Write via write() on the raw fd.
        // O_APPEND: seek-to-end + write are one indivisible kernel operation.
        (&self.0.file).write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // Data already in kernel page cache via write() above.
        // No userspace buffer remains — nothing to flush.
        Ok(())
    }
}

#[cfg(windows)]
impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Lock only for the WriteFile() syscall.
        // BufWriter's internal buffer was filled without holding this lock.
        match self.0.file.lock() {
            Ok(mut f) => f.write(buf),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Mutex poisoned",
            )),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // WriteFile() in write() already completed — nothing left to flush.
        Ok(())
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: Thread-local BufWriter
//
//  Each thread owns a `BufWriter<FileWriter>`.
//  `FileWriter` holds an `Arc<LogFile>` — used for both writing and rotation
//  detection (Arc::ptr_eq).
//
//  Lifecycle
//  ─────────
//  First log call   : `None` → `Some(BufWriter::with_capacity(…))`
//                     One heap allocation: BufWriter's internal byte buffer.
//  Normal log call  : writeln! into BufWriter's internal buffer — zero alloc.
//                     BufWriter flushes automatically when buffer is full.
//  Periodic flush   : FLUSH_FLAG seen → bw.flush() — no alloc.
//  Rotation         : Arc::ptr_eq mismatch → bw.flush() (old file) → replace.
//  Thread exit      : BufWriter::drop() calls flush() automatically.
//                     No custom Drop impl needed — stdlib handles it.
//
//  Why BufWriter::drop() replacing ThreadBuf::drop() is correct
//  ─────────────────────────────────────────────────────────────
//  BufWriter<W>::drop() calls self.flush() which calls W::write(buffered_bytes).
//  FileWriter::write() delivers bytes to the kernel (Unix) or acquires the
//  Mutex and calls WriteFile() (Windows). The effect is implemented by the
//  stdlib with no code on our side.
//
//  The one caveat: if flush() fails inside drop(), the error is silently
//  discarded.  This matches the behaviour of most I/O types in Rust and is
//  acceptable for a logger (the process is exiting anyway).
// ═════════════════════════════════════════════════════════════════════════════

thread_local! {
    static THREAD_WRITER: std::cell::RefCell<Option<BufWriter<FileWriter>>> =
        std::cell::RefCell::new(None);
}

/// Call `f` with the current thread's `BufWriter` if it exists.
fn with_thread_writer<F>(f: F)
where
    F: FnOnce(&mut BufWriter<FileWriter>),
{
    THREAD_WRITER.with(|cell| {
        if let Some(bw) = cell.borrow_mut().as_mut() {
            f(bw);
        }
    });
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: write_record
//
//  Hot path breakdown (steady state — no rotation, no flush flag):
//
//    Arc::ptr_eq()             1 pointer compare — branch not taken
//    FLUSH_FLAG.swap()         1 atomic op       — false, branch not taken
//    writeln!(bw, …)           extend_from_slice into BufWriter's Vec<u8>
//                              zero allocation while buffer has capacity
//    BufWriter internal check  if len >= capacity → FileWriter::write()
//                              one write() syscall per buffer-full event
//
//  Rotation path (Arc mismatch detected):
//    bw.flush()                FileWriter::write(remaining bytes) to OLD file
//    drop old BufWriter        BufWriter::drop() — another flush (no-op, already empty)
//    BufWriter::with_capacity  one heap allocation: new internal byte buffer
//    writeln!(bw, …)           first write into new buffer
//
//  `current: Arc<LogFile>` is cloned from ArcSwapOption::load_full() in Log::log().
//  load_full() increments the Arc refcount (atomic inc, ~1 ns, no heap alloc).
//  The Arc is either stored in the new BufWriter (rotation) or dropped at the
//  end of write_record (steady state, atomic dec, ~1 ns).
// ═════════════════════════════════════════════════════════════════════════════

fn write_record(record: &Record, current: Arc<LogFile>, capacity: usize, format: &LogFormat) {
    THREAD_WRITER.with(|cell| {
        let mut slot = cell.borrow_mut();

        // ── Rotation detection ────────────────────────────────────────────
        //
        // Compare the Arc inside the existing BufWriter's FileWriter with
        // the current Arc from the logger.  A pointer mismatch means reopen()
        // ran and swapped in a new LogFile since this thread last logged.
        //
        // ptr_eq compares the allocation address, not the contents —
        // O(1), no lock, no allocation.
        //
        // `slot.is_none()` covers first-use initialisation.
        let stale = slot
            .as_ref()
            .map_or(true, |bw| !Arc::ptr_eq(&bw.get_ref().0, &current));

        if stale {
            // Flush old BufWriter first so no bytes are lost.
            // If the writer is stale (rotation), flush() calls
            // FileWriter::write() which writes to the OLD file — correct,
            // those bytes belong to the pre-rotation period.
            // BufWriter is then dropped here (replaced by `*slot = Some(…)`),
            // which calls drop() → another flush(), but the buffer is already
            // empty so it is a no-op.
            if let Some(old_bw) = slot.as_mut() {
                let _ = old_bw.flush();
            }

            // Create a fresh BufWriter pointing to the current LogFile.
            // One heap allocation: BufWriter's internal byte buffer (capacity bytes).
            *slot = Some(BufWriter::with_capacity(
                capacity,
                FileWriter(Arc::clone(&current)),
            ));
        }

        // ── Periodic flush ────────────────────────────────────────────────
        // FLUSH_FLAG was set by the background flush thread.
        // Acquire ordering pairs with the Relaxed store in the flush thread.
        // swap(false) atomically reads and clears — one thread handles the flush,
        // others see false and skip it.
        //
        // We flush BEFORE writing the new record so the file sees a clean
        // time boundary: all previously buffered lines are written first,
        // then the new record follows immediately after.
        if FLUSH_FLAG.swap(false, Ordering::Acquire) {
            if let Some(bw) = slot.as_mut() {
                let _ = bw.flush();
                // bw.flush() calls FileWriter::write(buffered_bytes):
                //   Unix   : write() syscall via O_APPEND fd — atomic, no lock.
                //   Windows: Mutex<File>::lock() → WriteFile() → unlock.
                // FileWriter::flush() is then called but is a no-op.
            }
        }

        // ── Write the record into BufWriter's internal buffer ─────────────
        if let Some(bw) = slot.as_mut() {
            let _ = bw.write_all(format.render(record).as_bytes());
        }

        // `current` Arc drops here — atomic refcount decrement, no heap free
        // unless this was the last Arc to the old LogFile after a rotation.
    });
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: Logger
// ═════════════════════════════════════════════════════════════════════════════

pub struct MinimalLogger {
    default_level: LevelFilter,
    filters: Vec<TargetFilter>,
    file_path: Option<String>,
    buf_capacity: usize,
    #[allow(dead_code)]
    flush_ms: u64,
    format: LogFormat,
    file: ArcSwapOption<LogFile>,
}

static LOGGER: OnceLock<MinimalLogger> = OnceLock::new();

impl MinimalLogger {
    fn from_env() -> Self {
        let rust_log = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
        let file_path = std::env::var("RUST_LOG_FILE").ok();
        let buf_capacity = std::env::var("RUST_LOG_BUFFER_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_BUF_CAPACITY);
        let flush_ms = std::env::var("RUST_LOG_FLUSH_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_FLUSH_MS);

        let mut default_level = LevelFilter::Error;
        let mut filters = Vec::<TargetFilter>::new();

        for directive in rust_log.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match directive.split_once('=') {
                Some((target, level_str)) => match level_str.trim().parse::<LevelFilter>() {
                    Ok(level) => filters.push(TargetFilter {
                        target: target.trim().to_string(),
                        level,
                    }),
                    Err(_) => eprintln!(
                        "[minimal_logger] RUST_LOG: unknown level {:?} — skipping",
                        level_str
                    ),
                },
                None => match directive.parse::<LevelFilter>() {
                    Ok(level) => default_level = level,
                    Err(_) => eprintln!(
                        "[minimal_logger] RUST_LOG: unknown directive {:?} — skipping",
                        directive
                    ),
                },
            }
        }

        filters.sort_unstable_by(|a, b| b.target.len().cmp(&a.target.len()));

        let format = std::env::var("RUST_LOG_FORMAT")
            .map(|value| LogFormat::parse(&value))
            .unwrap_or_else(|_| LogFormat::parse(DEFAULT_LOG_FORMAT));

        let file = file_path.as_deref().and_then(|path| {
            let path = absolute(path)
                .unwrap_or(PathBuf::from(path))
                .display()
                .to_string();
            match open_log_file(&path) {
                Ok(f) => {
                    eprintln!(
                        "[minimal_logger] \"{path}\"  buf={buf_capacity}B/thread  \
                         flush={flush_ms}ms  writer=BufWriter<FileWriter>  \
                         drain={}  os={}",
                        if cfg!(windows) {
                            "Mutex<File>"
                        } else {
                            "O_APPEND"
                        },
                        std::env::consts::OS,
                    );
                    Some(Arc::new(LogFile::new(f, buf_capacity)))
                }
                Err(e) => {
                    eprintln!("[minimal_logger] Cannot open {path}: {e} — stderr fallback");
                    None
                }
            }
        });

        platform::register_rotation_handler();

        spawn_flush_thread(flush_ms);

        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            default_hook(info);
            let _ = std::panic::catch_unwind(shutdown);
        }));

        MinimalLogger {
            default_level,
            filters,
            file_path,
            buf_capacity,
            flush_ms,
            format,
            file: ArcSwapOption::new(file),
        }
    }

    #[inline]
    fn level_for(&self, target: &str) -> LevelFilter {
        self.filters
            .iter()
            .find(|f| target.starts_with(f.target.as_str()))
            .map(|f| f.level)
            .unwrap_or(self.default_level)
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: Reopen
// ═════════════════════════════════════════════════════════════════════════════

impl MinimalLogger {
    pub(crate) fn reopen(&self) {
        let Some(path) = self.file_path.as_deref() else {
            return;
        };

        // Open new file before swapping — no gap where self.file is None.
        let new_log_file = match open_log_file(path) {
            Ok(f) => Arc::new(LogFile::new(f, self.buf_capacity)),
            Err(e) => {
                eprintln!("[minimal_logger] reopen failed ({path}): {e} — keeping old file");
                return;
            }
        };

        // Atomic pointer swap.
        // write_record() detects the new Arc via Arc::ptr_eq on next log call
        // and flushes its old BufWriter to the old file before switching.
        let old = self.file.swap(Some(new_log_file));

        if let Some(old_arc) = old {
            match Arc::try_unwrap(old_arc) {
                Ok(old_log_file) => {
                    // Sole owner — no other thread holds an Arc to the old LogFile.
                    // All active BufWriters have either already flushed (if they
                    // detected the rotation via ptr_eq) or belong to threads that
                    // have not logged since the swap.
                    //
                    // flush_and_sync() on the old LogFile:
                    //   1. Flushes calling thread's BufWriter to the old file
                    //      (if the calling thread's writer still points to it).
                    //   2. sync_file():
                    //        Linux  : fdatasync()
                    //        macOS  : F_FULLFSYNC
                    //        Windows: FlushFileBuffers()
                    old_log_file.flush_and_sync();
                    drop(old_log_file); // fd closed explicitly
                    eprintln!("[minimal_logger] Old log file flushed, synced, and closed");
                }
                Err(still_shared) => {
                    // Other threads still hold an Arc inside their BufWriter's
                    // FileWriter.  They will detect the rotation on their next
                    // log call (Arc::ptr_eq mismatch), flush their BufWriter to
                    // the old file, then create a new BufWriter for the new file.
                    // When the last such thread releases its Arc, LogFile::drop()
                    // runs and the fd closes.
                    drop(still_shared);
                    eprintln!(
                        "[minimal_logger] Old log file has live BufWriters — will close when threads rotate"
                    );
                }
            }
        }

        eprintln!("[minimal_logger] Reopened: {path}");
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: File open helper
// ═════════════════════════════════════════════════════════════════════════════

fn open_log_file(path: &str) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true) // O_APPEND: atomic seek-to-end + write on Unix
        .open(path)
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: Background flush thread
// ═════════════════════════════════════════════════════════════════════════════

fn spawn_flush_thread(flush_ms: u64) {
    let result = std::thread::Builder::new()
        .name("minimal_logger-flush".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            let interval = Duration::from_millis(flush_ms);
            loop {
                std::thread::sleep(interval);
                // Relaxed: ordering is provided by the Acquire swap in write_record.
                FLUSH_FLAG.store(true, Ordering::Relaxed);
            }
        });

    if result.is_err() {
        eprintln!("[minimal_logger] Failed to spawn flush thread — periodic flush disabled");
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: Log trait
// ═════════════════════════════════════════════════════════════════════════════

impl Log for MinimalLogger {
    #[inline]
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level_for(metadata.target())
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        // Trigger reopen if signalled.  The actual per-thread BufWriter swap
        // happens lazily inside write_record() via Arc::ptr_eq detection —
        // each thread switches on its own next log call.
        if REOPEN_FLAG.swap(false, Ordering::Acquire) {
            self.reopen();
        }

        // load_full() clones the Arc — one atomic refcount increment (~1 ns).
        // No heap allocation.  Passes ownership to write_record() which either
        // stores it (on rotation) or drops it (steady state).
        match self.file.load_full() {
            Some(arc) => write_record(record, arc, self.buf_capacity, &self.format),
            None => {
                let stderr = std::io::stderr();
                let mut out = stderr.lock();
                let _ = out.write_all(self.format.render(record).as_bytes());
            }
        }
    }

    fn flush(&self) {
        // Flush calling thread's BufWriter.
        // bw.flush() → FileWriter::write(buffered_bytes) → write() / WriteFile()
        // FileWriter::flush() → no-op
        with_thread_writer(|bw| {
            let _ = bw.flush();
        });
        let _ = std::io::stderr().lock().flush();
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: Public API
// ═════════════════════════════════════════════════════════════════════════════

/// Initialise the logger and start the background flush thread.
/// Call once at program startup before any log macros are used.
pub fn init() -> Result<(), SetLoggerError> {
    let logger = LOGGER.get_or_init(MinimalLogger::from_env);

    let max = logger
        .filters
        .iter()
        .map(|f| f.level)
        .fold(logger.default_level, |acc, l| acc.max(l));

    log::set_logger(logger)?;
    log::set_max_level(max);

    Ok(())
}

/// Flush the calling thread's BufWriter to the kernel.
///
/// `BufWriter::drop()` on thread exit handles other threads automatically —
/// no explicit per-thread shutdown call is needed beyond the main thread.
pub fn shutdown() {
    if let Some(logger) = LOGGER.get() {
        logger.flush();
    }
}

#[cfg(test)]
mod tests;
