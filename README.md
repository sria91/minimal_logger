# minimal_logger

A minimal-resource, multi-platform logger for Rust applications with optional file output, 
automatic flushing, and log rotation support.

## Features

- Thread-local buffered logging with `BufWriter`
- Platform-specific log rotation support
  - Linux/macOS: `SIGHUP`
  - Windows: `Global\\RustLogger_LogRotate` event
- Periodic flush thread with configurable interval
- Environment-driven configuration for log level, output file, buffer size, and format
- Falls back to `stderr` when file output is unavailable

## Getting started

Add `minimal_logger` as a dependency and initialise it once at startup:

```rust
use log::{error, info};

fn main() {
    minimal_logger::init().expect("failed to initialise logger");

    info!("application started");
    error!("shutdown due to error");

    minimal_logger::shutdown();
}
```

## Configuration

The logger reads configuration from environment variables.

| Variable | Default | Description |
|---------|---------|-------------|
| `RUST_LOG` | `info` | Global level and per-target filters |
| `RUST_LOG_FILE` | `stderr` | Absolute path to log file; if unset, logs go to `stderr` |
| `RUST_LOG_BUFFER_SIZE` | `4096` | Per-thread buffer capacity in bytes |
| `RUST_LOG_FLUSH_MS` | `1000` | Periodic flush interval in milliseconds |
| `RUST_LOG_FORMAT` | `"{timestamp} [{level:<5}] T[{thread_name}] [{file}:{line}] {args}"` | Log message format template (timestamp is fixed 6-digit microseconds) |

Supported format fields:

- `timestamp`
- `level`
- `thread_name`
- `target`
- `module_path`
- `file`
- `line`
- `args`

## Rotation support

The logger can reopen its log file on demand using platform-native rotation signals:

- Unix-like systems: `SIGHUP`
- Windows: `Global\\RustLogger_LogRotate` named event

When rotation is triggered, existing thread-local buffers flush to the old file and new writes move to the reopened file.

## Cargo metadata

This crate exposes docs on [docs.rs](https://docs.rs/minimal_logger).
