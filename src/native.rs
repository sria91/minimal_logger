// ═════════════════════════════════════════════════════════════════════════════
//  PLATFORM: LINUX
// ═════════════════════════════════════════════════════════════════════════════
#[cfg(target_os = "linux")]
pub(crate) mod platform {
    use crate::REOPEN_FLAG;
    use std::sync::atomic::Ordering;

    extern "C" fn sighup_handler(_: libc::c_int) {
        REOPEN_FLAG.store(true, Ordering::Relaxed);
    }

    /// Register the `SIGHUP` handler via `sigaction(2)`.
    ///
    /// Uses `SA_RESTART` so that slow syscalls interrupted by the signal are
    /// automatically restarted instead of failing with `EINTR`.
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

    /// Sync file data to block storage, skipping metadata (`fdatasync`).
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
pub(crate) mod platform {
    use crate::REOPEN_FLAG;
    use std::sync::atomic::Ordering;

    extern "C" fn sighup_handler(_: libc::c_int) {
        REOPEN_FLAG.store(true, Ordering::Relaxed);
    }

    /// Register the `SIGHUP` handler via `sigaction(2)`.
    ///
    /// Uses `SA_RESTART` so that slow syscalls interrupted by the signal are
    /// automatically restarted instead of failing with `EINTR`.
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

    /// Sync file data through the disk controller cache (`F_FULLFSYNC`).
    ///
    /// Plain `fsync` on macOS only reaches the controller write cache.
    /// `F_FULLFSYNC` guarantees the data reaches physical media.
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
    fn to_wide_null(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// Spawn a background thread that waits on the `Global\\RustLogger_LogRotate`
    /// named event and sets `REOPEN_FLAG` whenever the event fires.
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

    /// Flush the OS write buffer for `file` to disk (`FlushFileBuffers`).
    pub fn sync_file(file: &std::fs::File) {
        use std::os::windows::io::AsRawHandle;
        let _ = unsafe { FlushFileBuffers(HANDLE(file.as_raw_handle())) };
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Fallback: other Unix-like platforms
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(all(unix, not(target_os = "linux"), not(target_os = "macos")))]
pub(crate) mod platform {
    use crate::REOPEN_FLAG;
    use std::sync::atomic::Ordering;

    extern "C" fn sighup_handler(_: libc::c_int) {
        REOPEN_FLAG.store(true, Ordering::Relaxed);
    }

    /// Register the `SIGHUP` handler via `sigaction(2)` on other Unix-like platforms.
    pub fn register_rotation_handler() {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = sighup_handler as libc::sighandler_t;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut());
        }
    }

    /// Sync file data to disk (`fsync`).
    pub fn sync_file(file: &std::fs::File) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::fsync(file.as_raw_fd());
        }
    }
}
