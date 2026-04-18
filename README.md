# minimal_logger

A minimal-resource, multi-platform logger for Rust with optional file output,
automatic flushing, platform-native log rotation, and change-aware runtime
reconfiguration via a builder API.

## Features

- Thread-local buffered logging with `BufWriter`
- Platform-specific log rotation
  - Linux / macOS: `SIGHUP`
  - Windows: `Global\\RustLogger_LogRotate` named event
- Periodic background flush thread with configurable interval
- Builder-based configuration for log level, output file, buffer size, and format
- Environment-variable bootstrap via `config_from_env()`
- Falls back to `stderr` when file output is unavailable
- Change-aware runtime reconfiguration — only updated subsystems are re-initialised

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
minimal_logger = "0.2"
log = "0.4"
```

## Getting started

Construct a `MinimalLoggerConfig` and pass it to `init()` once at startup:

```rust
use log::{error, info};

fn main() {
    minimal_logger::init(
        minimal_logger::MinimalLoggerConfig::new()
            .level(log::LevelFilter::Info)
    ).expect("failed to initialise logger");

    info!("application started");
    error!("shutdown due to error");

    minimal_logger::shutdown();
}
```

Or seed configuration from the standard `RUST_LOG*` environment variables:

```rust
fn main() {
    minimal_logger::init(minimal_logger::config_from_env())
        .expect("failed to initialise logger");
}
```

## Runtime reconfiguration

Call `reinit()` with a new `MinimalLoggerConfig` to update a running logger.

`reinit()` is change-aware and only updates components whose effective
configuration changed:

- log filters and max level (`.level()` / `.filter()`)
- output destination (`.file()` / `.stderr()`)
- periodic flush worker interval (`.flush_ms()`)
- rendering format (`.format()`)
- per-thread buffer capacity for newly recreated writers (`.buf_capacity()`)

Unset fields keep their **current** value — you can change a single subsystem
without repeating the full configuration. If nothing changed, `reinit()` returns
immediately.

```rust
use log::info;

fn reconfigure() {
    // Switch to debug level; all other settings unchanged.
    minimal_logger::reinit(
        minimal_logger::MinimalLoggerConfig::new()
            .level(log::LevelFilter::Debug)
            .filter("myapp::db", log::LevelFilter::Trace)
    );
    info!("logger reconfigured");
}
```

## Configuration

| Builder method     | Default at `init` | Description                                          |
|--------------------|-------------------|------------------------------------------------------|
| `.level(l)`        | `Info`            | Global log level                                     |
| `.filter(t, l)`    | *(none)*          | Per-target level override; may be called many times  |
| `.file(path)`      | *(stderr)*        | Append log records to a file (`O_APPEND`)            |
| `.stderr()`        | *(default)*       | Explicitly route output back to stderr               |
| `.buf_capacity(n)` | `4096`            | Per-thread `BufWriter` capacity in bytes             |
| `.flush_ms(ms)`    | `1000`            | Periodic flush interval in milliseconds              |
| `.format(tmpl)`    | *see below*       | Log-line template with `{field}` placeholders        |

### Level and filter syntax

```rust
// All targets at DEBUG
MinimalLoggerConfig::new().level(log::LevelFilter::Debug)

// Global WARN; myapp at DEBUG
MinimalLoggerConfig::new()
    .level(log::LevelFilter::Warn)
    .filter("myapp", log::LevelFilter::Debug)

// Layered per-module overrides
MinimalLoggerConfig::new()
    .level(log::LevelFilter::Info)
    .filter("myapp::db", log::LevelFilter::Trace)
    .filter("hyper", log::LevelFilter::Off)
```

Recognised levels: `Off`, `Error`, `Warn`, `Info`, `Debug`, `Trace`.
When multiple filters match a target the most specific (longest prefix) wins.

### Format fields

| Placeholder      | Example output                |
|------------------|-------------------------------|
| `{timestamp}`    | `2026-04-18T12:34:56.789012Z` |
| `{level}`        | `INFO`                        |
| `{thread_name}`  | `main`                        |
| `{target}`       | `myapp::server`               |
| `{module_path}`  | `myapp::server`               |
| `{file}`         | `src/server.rs`               |
| `{line}`         | `42`                          |
| `{args}`         | `listening on :8080`          |

Width and alignment: `{level:<5}` left-aligns in a field of width 5;
`{line:>4}` right-aligns. Use `{{` and `}}` for literal brace characters.

Default format string:

```text
{timestamp} [{level:<5}] T[{thread_name}] [{file}:{line}] {args}
```

## Log rotation

The logger reopens its log file on a platform-native signal or event, with no
gap in output and no lost bytes.

| Platform      | Trigger                                   |
|---------------|-------------------------------------------|
| Linux / macOS | `SIGHUP`                                  |
| Other Unix    | `SIGHUP`                                  |
| Windows       | `Global\RustLogger_LogRotate` named event |

When the signal fires, each thread detects the new file on its **next log call**
via an `Arc` pointer comparison: it flushes its buffered bytes to the old file
descriptor, then creates a new `BufWriter` pointing to the freshly opened file.
The old file descriptor is closed once no thread holds a reference to it.

Example logrotate configuration (Linux):

```text
/var/log/myapp.log {
    daily
    rotate 7
    postrotate
        kill -HUP $(cat /var/run/myapp.pid)
    endscript
}
```

## API summary

| Function                        | Description                                                        |
|---------------------------------|--------------------------------------------------------------------|
| `init(MinimalLoggerConfig)`     | Register the logger and start the flush worker. Call once at startup. |
| `reinit(MinimalLoggerConfig)`   | Apply a new config; update only changed subsystems.               |
| `config_from_env()`             | Build a `MinimalLoggerConfig` from `RUST_LOG*` environment variables. |
| `shutdown()`                    | Flush the calling thread's buffered writer before process exit.    |

## Cargo metadata

This crate exposes docs on [docs.rs](https://docs.rs/minimal_logger).
