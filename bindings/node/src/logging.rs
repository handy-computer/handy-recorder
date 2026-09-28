//! Forwards the library's `log` records to a JavaScript handler.
//!
//! One handler per process: the `log` crate takes one logger, installed on
//! the first `setLogHandler`; later calls swap the JavaScript target. With no
//! handler the maximum level is `Off`, so the library's log calls cost a
//! comparison and nothing crosses into JavaScript.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};
use napi::bindgen_prelude::{Function, Unknown};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{Error as NapiError, Result, Status};
use napi_derive::napi;

/// Records held for a busy event loop; past this they're dropped and counted.
const QUEUE: usize = 1024;

/// Weak, like the recorder's events: logging never keeps the process alive.
type Handler =
    ThreadsafeFunction<JsLogRecord, Unknown<'static>, JsLogRecord, Status, false, true, QUEUE>;

/// A log record as the handler receives it.
#[napi(object, object_from_js = false)]
pub struct JsLogRecord {
    /// `error`, `warn`, `info`, `debug`, or `trace`.
    pub level: String,
    /// The Rust module that wrote it, e.g. `handy_recorder::capture::engine`.
    pub target: String,
    pub message: String,
    /// When it was written, in milliseconds since the Unix epoch, not when
    /// JavaScript received it.
    pub time_ms: f64,
}

struct JsLogger {
    handler: Mutex<Option<Handler>>,
    /// Records the full queue refused since the last one delivered.
    dropped: AtomicU64,
}

static LOGGER: JsLogger = JsLogger {
    handler: Mutex::new(None),
    dropped: AtomicU64::new(0),
};

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64() * 1000.0)
}

fn record(level: Level, target: &str, message: String) -> JsLogRecord {
    JsLogRecord {
        level: level.as_str().to_ascii_lowercase(),
        target: target.into(),
        message,
        time_ms: now_ms(),
    }
}

impl Log for JsLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, entry: &Record) {
        if !self.enabled(entry.metadata()) {
            return;
        }
        // Formatted before taking the lock, and timed when written.
        let entry = record(entry.level(), entry.target(), entry.args().to_string());
        let mut handler = self
            .handler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(tsfn) = handler.as_ref() else {
            return;
        };
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            let notice = record(
                Level::Warn,
                module_path!(),
                format!("dropped {dropped} log records while the event loop was busy"),
            );
            if tsfn.call(notice, ThreadsafeFunctionCallMode::NonBlocking) != Status::Ok {
                self.dropped.fetch_add(dropped + 1, Ordering::Relaxed);
                return;
            }
        }
        match tsfn.call(entry, ThreadsafeFunctionCallMode::NonBlocking) {
            Status::Ok => {}
            Status::QueueFull => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            // The handler's environment is gone (its worker exited).
            _ => {
                *handler = None;
                log::set_max_level(LevelFilter::Off);
            }
        }
    }

    fn flush(&self) {}
}

fn level_filter(level: &str) -> Result<LevelFilter> {
    match level {
        "error" => Ok(LevelFilter::Error),
        "warn" => Ok(LevelFilter::Warn),
        "info" => Ok(LevelFilter::Info),
        "debug" => Ok(LevelFilter::Debug),
        "trace" => Ok(LevelFilter::Trace),
        _ => Err(NapiError::new(
            Status::InvalidArg,
            format!("unknown log level {level:?}"),
        )),
    }
}

/// Sends log records at `level` and above to `handler`; `null` stops them.
#[napi(js_name = "setLogHandler")]
pub fn set_log_handler(
    handler: Option<Function<'_, JsLogRecord, Unknown<'static>>>,
    level: String,
) -> Result<()> {
    let filter = level_filter(&level)?;
    let tsfn = handler
        .map(|handler| {
            handler
                .build_threadsafe_function::<JsLogRecord>()
                .weak::<true>()
                .callee_handled::<false>()
                .max_queue_size::<QUEUE>()
                .build_callback(|ctx| Ok(ctx.value))
        })
        .transpose()?;

    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        // Fails only if this addon already set one, which it does only here.
        let _ = log::set_logger(&LOGGER);
    });

    let enabled = tsfn.is_some();
    // Off first, so no record reaches a handler at the wrong level.
    log::set_max_level(LevelFilter::Off);
    *LOGGER
        .handler
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = tsfn;
    LOGGER.dropped.store(0, Ordering::Relaxed);
    if enabled {
        log::set_max_level(filter);
    }
    Ok(())
}
