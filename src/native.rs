// ═════════════════════════════════════════════════════════════════════════════
//  PLATFORM: Unix (Linux, macOS, and other Unix-like systems)
//
//  All three Unix variants share an identical SIGHUP handler and sigaction
//  registration.  Only `sync_file` differs — macOS requires F_FULLFSYNC to
//  flush through the disk controller cache, Linux uses fdatasync, and the
//  generic fallback uses plain fsync.
// ═════════════════════════════════════════════════════════════════════════════

// ─────────────────────────────────────────────────────────────────────────────
//  Shared Unix signal handler and sigaction registration
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(unix)]
mod unix_signal {
    use crate::REOPEN_FLAG;
    use std::sync::atomic::Ordering;

    pub(super) extern "C" fn sighup_handler(_: libc::c_int) {
        // Signal-safe: only stores to an AtomicBool.
        REOPEN_FLAG.store(true, Ordering::Relaxed);
    }

    /// Register the `SIGHUP` handler via `sigaction(2)`.
    ///
    /// Uses `SA_RESTART` so that slow syscalls interrupted by the signal are
    /// automatically restarted instead of failing with `EINTR`.
    pub(super) fn register_sighup() {
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
}

// ─────────────────────────────────────────────────────────────────────────────
//  Linux platform module
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(target_os = "linux")]
pub(crate) mod platform {
    use super::unix_signal;

    pub fn register_rotation_handler() {
        unix_signal::register_sighup();
    }

    /// Sync file data to block storage, skipping metadata (`fdatasync`).
    ///
    /// Logs a warning if `fdatasync` fails.
    pub fn sync_file(file: &std::fs::File) {
        use std::os::unix::io::AsRawFd;
        let ret = unsafe { libc::fdatasync(file.as_raw_fd()) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            eprintln!("[minimal_logger] fdatasync failed: {err}");
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  macOS platform module
//
//  F_FULLFSYNC required — plain fsync() only reaches the controller cache.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(target_os = "macos")]
pub(crate) mod platform {
    use super::unix_signal;

    pub fn register_rotation_handler() {
        unix_signal::register_sighup();
    }

    /// Sync file data through the disk controller cache (`F_FULLFSYNC`).
    ///
    /// Plain `fsync` on macOS only reaches the controller write cache.
    /// `F_FULLFSYNC` guarantees the data reaches physical media.
    /// Logs a warning if `fcntl(F_FULLFSYNC)` fails.
    pub fn sync_file(file: &std::fs::File) {
        use std::os::unix::io::AsRawFd;
        let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            eprintln!("[minimal_logger] F_FULLFSYNC failed: {err}");
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  PLATFORM: WINDOWS
// ═════════════════════════════════════════════════════════════════════════════
#[cfg(windows)]
pub(crate) mod platform {
    use crate::REOPEN_FLAG;
    use std::sync::atomic::Ordering;
    use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::FlushFileBuffers;
    use windows::Win32::System::Threading::{
        CreateEventW, INFINITE, ResetEvent, WaitForSingleObject,
    };
    use windows::core::PCWSTR;

    /// Encode a UTF-8 string as a null-terminated UTF-16 sequence for Win32 APIs.
    ///
    /// Unlike a raw `encode_utf16().collect()`, this function appends the
    /// required `0u16` sentinel so callers do not need to embed `\0` in string
    /// literals (which is error-prone and hard to audit).
    fn to_wide_null(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0u16)).collect()
    }

    /// Spawn a background thread that waits on the `Local\RustLogger_LogRotate`
    /// named event and sets `REOPEN_FLAG` whenever the event fires.
    ///
    /// The event is placed in the session-local (`Local\`) namespace rather than
    /// `Global\` to prevent processes in other user sessions from triggering an
    /// unintended log-file rotation.
    pub fn register_rotation_handler() {
        let name = to_wide_null("Local\\RustLogger_LogRotate");
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

    /// Flush the OS write buffer for `file` to disk (`FlushFileBuffers`).
    ///
    /// Logs a warning if `FlushFileBuffers` fails.
    pub fn sync_file(file: &std::fs::File) {
        use std::os::windows::io::AsRawHandle;
        if let Err(e) = unsafe { FlushFileBuffers(HANDLE(file.as_raw_handle())) } {
            eprintln!("[minimal_logger] FlushFileBuffers failed: {e}");
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Fallback: other Unix-like platforms (not Linux, not macOS)
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(all(unix, not(target_os = "linux"), not(target_os = "macos")))]
pub(crate) mod platform {
    use super::unix_signal;

    pub fn register_rotation_handler() {
        unix_signal::register_sighup();
    }

    /// Sync file data to disk (`fsync`).
    ///
    /// Logs a warning if `fsync` fails.
    pub fn sync_file(file: &std::fs::File) {
        use std::os::unix::io::AsRawFd;
        let ret = unsafe { libc::fsync(file.as_raw_fd()) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            eprintln!("[minimal_logger] fsync failed: {err}");
        }
    }
}
