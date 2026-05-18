use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::{Arc, Mutex};

use log::Record;

use crate::{config::LogFormat, platform, report_io_error};

/// An open log file with a shared buffered writer.
///
/// A single `BufWriter<File>` is shared by all threads via a mutex so periodic
/// flushes can drain buffered records deterministically without waiting for the
/// original producer thread to log again.
pub(crate) struct LogFile {
    writer: Mutex<BufWriter<File>>,
}

impl LogFile {
    /// Wrap an open file in a `LogFile` with the given `BufWriter` capacity hint.
    pub(crate) fn new(file: File, buf_capacity: usize) -> Self {
        LogFile {
            writer: Mutex::new(BufWriter::with_capacity(buf_capacity, file)),
        }
    }

    /// Flush buffered records to the OS page cache.
    pub(crate) fn flush(&self) {
        let mut writer = match self.writer.lock() {
            Ok(writer) => writer,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Err(err) = writer.flush() {
            report_io_error("flush buffered log records", &err);
        }
    }

    /// Flush buffered records and sync the file to physical media.
    pub(crate) fn flush_and_sync(&self) {
        let mut writer = match self.writer.lock() {
            Ok(writer) => writer,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Err(err) = writer.flush() {
            report_io_error("flush buffered log records", &err);
        }
        platform::sync_file(writer.get_ref());
    }
}

/// No-op compatibility helper retained for call sites that transition to stderr.
pub(crate) fn flush_and_clear_thread_writer() {}

// ═════════════════════════════════════════════════════════════════════════════
//  COMMON: write_record
// ═════════════════════════════════════════════════════════════════════════════

/// Write a single log `record` to the shared buffered file writer.
pub(crate) fn write_record(
    record: &Record,
    current: Arc<LogFile>,
    _capacity: usize,
    flush_every_record: bool,
    format: &LogFormat,
) {
    let mut writer = match current.writer.lock() {
        Ok(writer) => writer,
        Err(poisoned) => poisoned.into_inner(),
    };

    if let Err(err) = writer.write_all(format.render(record).as_bytes()) {
        report_io_error("write log record", &err);
    }
    if flush_every_record && let Err(err) = writer.flush() {
        report_io_error("flush log record", &err);
    }
}
