use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use log::Record;

use crate::{FLUSH_FLAG, config::LogFormat, platform};

// ═════════════════════════════════════════════════════════════════════════════
//  UNIX: LogFile — bare File, no Mutex
// ═════════════════════════════════════════════════════════════════════════════

/// An open log file used for `O_APPEND` writes on Unix.
///
/// Wrapped in an `Arc` that is shared by all thread-local `BufWriter<FileWriter>`
/// instances currently pointing at this file. The `Arc` pointer identity is the
/// rotation detector: a mismatch between a thread's cached pointer and the
/// logger's current pointer means the file was rotated since that thread last logged.
#[cfg(unix)]
pub(crate) struct LogFile {
    /// Underlying file descriptor opened with `O_APPEND | O_CREAT`.
    pub(crate) file: File,
    /// Per-thread `BufWriter` capacity forwarded to replacement writers after rotation.
    #[allow(dead_code)]
    pub(crate) buf_capacity: usize,
}

#[cfg(unix)]
impl LogFile {
    /// Wrap an open file in a `LogFile` with the given `BufWriter` capacity hint.
    pub(crate) fn new(file: File, buf_capacity: usize) -> Self {
        LogFile { file, buf_capacity }
    }

    /// Flush the calling thread's `BufWriter` to this file, then sync to physical media.
    ///
    /// Called by [`MinimalLogger::reopen`] after `Arc::try_unwrap` succeeds, meaning
    /// this thread is the last holder of the old `LogFile`.
    pub(crate) fn flush_and_sync(&self) {
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

/// An open log file used for serialised writes on Windows.
///
/// The inner `Mutex<File>` serialises concurrent `WriteFile` calls because
/// Windows does not provide the `O_APPEND` atomicity guarantee that Unix does.
/// The mutex is held only for the duration of each `WriteFile` syscall;
/// all formatting work occurs in the per-thread `BufWriter` beforehand.
#[cfg(windows)]
pub(crate) struct LogFile {
    /// File protected by a mutex to serialise `WriteFile` syscalls.
    pub(crate) file: Mutex<File>,
    /// Per-thread `BufWriter` capacity forwarded to replacement writers after rotation.
    #[allow(dead_code)]
    pub(crate) buf_capacity: usize,
}

#[cfg(windows)]
impl LogFile {
    /// Wrap an open file in a `LogFile` with the given `BufWriter` capacity hint.
    pub(crate) fn new(file: File, buf_capacity: usize) -> Self {
        LogFile {
            file: Mutex::new(file),
            buf_capacity,
        }
    }

    /// Flush the calling thread's `BufWriter` to this file, then sync to physical media.
    ///
    /// Called by [`MinimalLogger::reopen`] after `Arc::try_unwrap` succeeds.
    pub(crate) fn flush_and_sync(&self) {
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

/// The `W` in `BufWriter<W>` — bridges the per-thread buffer to a shared [`LogFile`].
///
/// Holds an `Arc<LogFile>` that doubles as a rotation detector: on every log
/// call, `write_record` compares this pointer with the logger's current `Arc`
/// via [`Arc::ptr_eq`]. A mismatch means the file was rotated since this thread
/// last logged, and the old buffer is flushed to the old file before switching.
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

/// Invoke `f` with the calling thread's `BufWriter`, if one has been initialised.
///
/// Does nothing when the thread has not yet emitted any log records.
pub(crate) fn with_thread_writer<F>(f: F)
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

/// Write a single log `record` to the calling thread's `BufWriter`.
///
/// Handles three concerns in sequence:
/// 1. **Rotation detection** — if the thread's cached `Arc<LogFile>` no longer
///    matches the logger's current pointer, the old writer is flushed and
///    replaced with a fresh one pointing to the new file.
/// 2. **Periodic flush** — if `FLUSH_FLAG` is set by the background worker, the
///    buffer is flushed before appending the new record.
/// 3. **Write** — the rendered record bytes are appended to the buffer.
pub(crate) fn write_record(
    record: &Record,
    current: Arc<LogFile>,
    capacity: usize,
    format: &LogFormat,
) {
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
