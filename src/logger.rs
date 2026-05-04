use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex, Once, OnceLock,
        atomic::Ordering,
        mpsc::{self, RecvTimeoutError, Sender},
    },
    thread::JoinHandle,
    time::Duration,
};

use arc_swap::{ArcSwap, ArcSwapOption};
use log::{Log, Metadata, Record};

use crate::{
    FLUSH_FLAG, MinimalLoggerConfig, REOPEN_FLAG,
    config::{ActiveConfig, ReloadConfig},
    log_file::{LogFile, flush_and_clear_thread_writer, with_thread_writer, write_record},
    native::platform,
    report_io_error, shutdown,
};

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: Logger
// ═════════════════════════════════════════════════════════════════════════════

/// The global logger singleton.
///
/// This type is not constructed or used directly. Call [`init()`] to register
/// it and [`reinit()`] to update its configuration.
pub(crate) struct MinimalLogger {
    pub(crate) state: ArcSwap<ActiveConfig>,
    pub(crate) file: ArcSwapOption<LogFile>,
    pub(crate) flush_worker: Mutex<FlushWorker>,
}

/// The process-wide logger singleton, registered with the `log` facade by [`init()`].
pub(crate) static LOGGER: OnceLock<&'static MinimalLogger> = OnceLock::new();
/// Guards one-time process setup: the rotation signal handler and the panic flush hook.
static RUNTIME_INIT: Once = Once::new();

/// A background thread that periodically sets `FLUSH_FLAG` to trigger buffered writes.
///
/// Replaced (stopped + joined + respawned) when `RUST_LOG_FLUSH_MS` changes;
/// stopped and joined during [`shutdown()`].
pub(crate) struct FlushWorker {
    /// Notifies the worker to exit immediately, without waiting for the interval.
    stop_tx: Option<Sender<()>>,
    /// Handle to the spawned thread; `None` if spawning failed.
    handle: Option<JoinHandle<()>>,
}

impl FlushWorker {
    /// Spawn a flush worker that sets `FLUSH_FLAG` every `flush_ms` milliseconds.
    ///
    /// When `flush_ms == 0` no thread is spawned; per-record flushing is handled
    /// synchronously by `write_record`. This prevents a tight busy-spin that
    /// would otherwise burn a CPU core.
    ///
    /// If the thread cannot be spawned, prints a warning to stderr and returns a
    /// no-op worker; periodic flushing will be disabled for that interval.
    fn spawn(flush_ms: u64) -> Self {
        FLUSH_FLAG.store(false, Ordering::Relaxed);

        if flush_ms == 0 {
            return FlushWorker {
                stop_tx: None,
                handle: None,
            };
        }

        let (stop_tx, stop_rx) = mpsc::channel();

        let handle = std::thread::Builder::new()
            .name("minimal_logger-flush".into())
            .stack_size(64 * 1024)
            .spawn(move || {
                let interval = Duration::from_millis(flush_ms);
                loop {
                    match stop_rx.recv_timeout(interval) {
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => FLUSH_FLAG.store(true, Ordering::Release),
                    }
                }
            })
            .ok();

        if handle.is_none() {
            eprintln!("[minimal_logger] Failed to spawn flush thread — periodic flush disabled");
        }

        FlushWorker {
            stop_tx: handle.as_ref().map(|_| stop_tx),
            handle,
        }
    }

    /// Signal the worker to exit and block until the thread has joined.
    ///
    /// After this returns the worker is inert and can be replaced.
    fn stop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            match handle.join() {
                Ok(()) => {}
                Err(_) => eprintln!("[minimal_logger] Flush worker panicked during shutdown"),
            }
        }
    }

    /// Test helper: call `spawn` with a given `flush_ms` without going through
    /// the full logger initialisation. Allows unit tests to inspect `has_handle`.
    #[cfg(test)]
    pub(crate) fn spawn_for_test(flush_ms: u64) -> Self {
        Self::spawn(flush_ms)
    }

    /// Returns `true` if this worker has a live background thread handle.
    #[cfg(test)]
    pub(crate) fn has_handle(&self) -> bool {
        self.handle.is_some()
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: Public configuration builder
// ═════════════════════════════════════════════════════════════════════════════

/// The output destination for log records.
///
/// Used inside [`MinimalLoggerConfig`] to distinguish "explicitly set to stderr" from
/// "not set" (which inherits the current file on [`reinit()`], or defaults to
/// stderr on [`init()`]).
pub(crate) enum FileTarget {
    /// Write to standard error.
    Stderr,
    /// Write to the file at the given path (created if it does not exist).
    Path(PathBuf),
}

/// Install process-global hooks on the first call; all subsequent calls are no-ops.
///
/// Sets up the platform rotation handler (SIGHUP / named event) and installs a
/// panic hook that calls [`shutdown()`] before delegating to the previous hook.
pub(crate) fn install_runtime_hooks_once() {
    RUNTIME_INIT.call_once(|| {
        platform::register_rotation_handler();

        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            default_hook(info);
            let _ = std::panic::catch_unwind(shutdown);
        }));
    });
}

/// Open the log file specified in `cfg`, if any, and wrap it in an `Arc<LogFile>`.
///
/// Returns `None` when `cfg.file_path` is `None` (stderr mode) or when the file
/// cannot be opened; in the latter case a diagnostic is printed to stderr.
fn load_file_for_config(cfg: &ReloadConfig) -> Option<Arc<LogFile>> {
    cfg.file_path.as_deref().and_then(|path| match open_log_file(path) {
        Ok(f) => {
            eprintln!(
                "[minimal_logger] \"{path}\"  buf={}B/thread  flush={}ms  writer=BufWriter<FileWriter>  drain={}  os={}",
                cfg.buf_capacity,
                cfg.flush_ms,
                if cfg!(windows) {
                    "Mutex<File>"
                } else {
                    "O_APPEND"
                },
                std::env::consts::OS,
            );
            Some(Arc::new(LogFile::new(f, cfg.buf_capacity)))
        }
        Err(e) => {
            eprintln!("[minimal_logger] Cannot open {path}: {e} — stderr fallback");
            None
        }
    })
}

impl MinimalLogger {
    /// Construct a new `MinimalLogger` from a [`MinimalLoggerConfig`] builder.
    ///
    /// Opens the log file (if configured), starts the flush worker, and
    /// installs the initial active configuration.
    pub(crate) fn from_config(config: MinimalLoggerConfig) -> Self {
        let reload = config.into_reload(None);
        let active = Arc::new(ActiveConfig::from_reload(reload.clone()));

        MinimalLogger {
            state: ArcSwap::from(active),
            file: ArcSwapOption::new(load_file_for_config(&reload)),
            flush_worker: Mutex::new(FlushWorker::spawn(reload.flush_ms)),
        }
    }

    /// Signal the flush worker to exit and wait for it to finish.
    ///
    /// Called by [`shutdown()`] so that no further `FLUSH_FLAG` stores can
    /// race with the process tearing down.
    pub(crate) fn stop_flush_worker(&self) {
        if let Ok(mut worker) = self.flush_worker.lock() {
            worker.stop();
        }
    }

    /// Release the old `Arc<LogFile>` after a file swap, logging the outcome.
    ///
    /// Extracted to a single place so that `swap_file_handle` and `reopen`
    /// both produce identical diagnostic messages.
    fn release_old_log_file(old_arc: Arc<LogFile>) {
        match Arc::try_unwrap(old_arc) {
            Ok(old_log_file) => {
                old_log_file.flush_and_sync();
                drop(old_log_file);
                eprintln!("[minimal_logger] Old log file flushed, synced, and closed");
            }
            Err(still_shared) => {
                drop(still_shared);
                eprintln!(
                    "[minimal_logger] Old log file has live BufWriters — will close when threads rotate"
                );
            }
        }
    }

    /// Atomically replace the active `LogFile`, flushing and syncing the old one.
    ///
    /// If this is the last `Arc` for the old file it is immediately flushed,
    /// synced, and closed. Otherwise the old file stays open until all threads
    /// holding a reference rotate to the new one on their next log call.
    fn swap_file_handle(&self, replacement: Option<Arc<LogFile>>) {
        let old = self.file.swap(replacement);
        if let Some(old_arc) = old {
            Self::release_old_log_file(old_arc);
        }
    }

    /// Replace the flush worker if and only if `old_ms != new_ms`.
    ///
    /// Acquires the worker mutex, signals the running thread to stop, joins it,
    /// then installs a fresh worker with the new interval.
    fn maybe_replace_flush_worker(&self, old_ms: u64, new_ms: u64) {
        if old_ms == new_ms {
            return;
        }

        let mut worker = match self.flush_worker.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        worker.stop();
        *worker = FlushWorker::spawn(new_ms);
    }

    /// Apply a new [`ReloadConfig`], updating only the parts that changed.
    ///
    /// Returns immediately when the parsed environment is identical to the
    /// current configuration (full equality check via `PartialEq`).
    pub(crate) fn apply_reload(&self, next_reload: ReloadConfig) {
        let current = self.state.load();
        if current.reload == next_reload {
            return;
        }

        let old_reload = current.reload.clone();
        drop(current);

        if old_reload.file_path != next_reload.file_path {
            self.swap_file_handle(load_file_for_config(&next_reload));
            if let Some(path) = next_reload.file_path.as_deref() {
                eprintln!("[minimal_logger] Reconfigured output file: {path}");
            } else {
                flush_and_clear_thread_writer();
                eprintln!("[minimal_logger] Reconfigured output to stderr");
            }
        }

        self.maybe_replace_flush_worker(old_reload.flush_ms, next_reload.flush_ms);

        let next_active = Arc::new(ActiveConfig::from_reload(next_reload));
        log::set_max_level(next_active.max_level);
        self.state.store(next_active);
    }
}

impl Drop for MinimalLogger {
    fn drop(&mut self) {
        match self.flush_worker.get_mut() {
            Ok(worker) => worker.stop(),
            Err(poisoned) => poisoned.into_inner().stop(),
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: Reopen
// ═════════════════════════════════════════════════════════════════════════════

impl MinimalLogger {
    /// Reopen the log file in response to a rotation signal.
    ///
    /// Opens a fresh file descriptor at the same path before atomically swapping
    /// out the old one, ensuring no log records are lost during the transition.
    /// Called from `Log::log()` when `REOPEN_FLAG` is set by the platform handler.
    pub(crate) fn reopen(&self) {
        let state = self.state.load();
        let Some(path) = state.reload.file_path.clone() else {
            return;
        };

        // Open new file before swapping — no gap where self.file is None.
        let new_log_file = match open_log_file(&path) {
            Ok(f) => Arc::new(LogFile::new(f, state.reload.buf_capacity)),
            Err(e) => {
                eprintln!("[minimal_logger] reopen failed ({path}): {e} — keeping old file");
                return;
            }
        };

        drop(state);

        // Atomic pointer swap.
        // write_record() detects the new Arc via Arc::ptr_eq on next log call
        // and flushes its old BufWriter to the old file before switching.
        let old = self.file.swap(Some(new_log_file));

        if let Some(old_arc) = old {
            Self::release_old_log_file(old_arc);
        }

        eprintln!("[minimal_logger] Reopened: {path}");
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: File open helper
// ═════════════════════════════════════════════════════════════════════════════

/// Open or create the log file at `path` in append mode.
///
/// On Unix, `O_APPEND` makes each `write()` syscall atomic with respect to
/// concurrent writers sharing the same file descriptor. On Windows, a
/// `Mutex<File>` inside `FileWriter` serialises `WriteFile` calls instead.
fn open_log_file(path: &str) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true); // O_APPEND: atomic seek-to-end + write on Unix

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // New log files should not be world-readable by default. Existing file
        // permissions are left unchanged by `open`.
        options.mode(0o600);
    }

    options.open(path)
}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: Log trait
// ═════════════════════════════════════════════════════════════════════════════

impl Log for MinimalLogger {
    #[inline]
    fn enabled(&self, metadata: &Metadata) -> bool {
        let state = self.state.load();
        metadata.level() <= state.level_for(metadata.target())
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

        let state = self.state.load();
        let buf_capacity = state.reload.buf_capacity;
        let flush_every_record = state.reload.flush_ms == 0;
        let format = state.format.clone();
        drop(state);

        match self.file.load_full() {
            Some(arc) => write_record(record, arc, buf_capacity, flush_every_record, &format),
            None => {
                // If this thread used to write to a file, flush and drop its
                // stale thread-local writer before switching to stderr output.
                flush_and_clear_thread_writer();

                let stderr = std::io::stderr();
                let mut out = stderr.lock();
                let flush_requested = FLUSH_FLAG.swap(false, Ordering::Acquire);
                if let Err(err) = out.write_all(format.render(record).as_bytes()) {
                    report_io_error("write log record to stderr", &err);
                }
                if (flush_requested || flush_every_record)
                    && let Err(err) = out.flush()
                {
                    report_io_error("flush stderr", &err);
                }
            }
        }
    }

    fn flush(&self) {
        // Flush calling thread's BufWriter.
        // bw.flush() → FileWriter::write(buffered_bytes) → write() / WriteFile()
        // FileWriter::flush() → no-op
        with_thread_writer(|bw| {
            if let Err(err) = bw.flush() {
                report_io_error("flush buffered log records", &err);
            }
        });
        if let Err(err) = std::io::stderr().lock().flush() {
            report_io_error("flush stderr", &err);
        }
    }
}
