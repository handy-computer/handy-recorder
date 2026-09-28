//! Node.js bindings for handy-recorder. The JavaScript wrapper (`index.js`)
//! is the public API; this is its native half.
//!
//! Everything that can block (listing devices, open, stop, close) runs on a
//! thread of its own, never the JavaScript thread. Everything the recorder
//! reports back, audio chunks included, goes through one threadsafe function
//! per recorder, so JavaScript sees events in the order they happened: every
//! chunk of a recording arrives before the `stopped` event. The exception is
//! `failed`: the library calls the failure handler from a thread of its own,
//! so it may arrive before `opened` or after `stopped` or `closed`. Calls into
//! JavaScript never wait for it; a busy event loop queues events, it doesn't
//! stall the sink.
//!
//! The library's `log` records reach JavaScript the same way, through one
//! process-wide handler (`logging`), stamped with when they were written.
//!
//! Tested from JavaScript (`test/`). napi doesn't register exports in a
//! `cargo test` build, where they would all read as dead code.
#![cfg_attr(test, allow(dead_code))]

mod convert;
#[cfg(feature = "test-backend")]
mod fake;
mod logging;
mod recorder;

use std::thread;

use napi::bindgen_prelude::Object;
use napi::{Env, Error as NapiError, Result};
use napi_derive::napi;

pub use convert::{JsErrorInfo, JsInputDevice};

/// Input devices, or the error listing them failed with.
#[napi(object, object_from_js = false)]
pub struct JsDeviceList {
    pub devices: Vec<JsInputDevice>,
    pub error: Option<JsErrorInfo>,
}

/// Lists input devices off the JavaScript thread, since the platform can take
/// a while. Resolves to a `JsDeviceList`.
#[napi(js_name = "listInputDevices", ts_return_type = "Promise<JsDeviceList>")]
pub fn list_input_devices(env: &Env) -> Result<Object<'_>> {
    let (deferred, promise) = env.create_deferred()?;
    thread::Builder::new()
        .name("handy-recorder-node-list".into())
        .spawn(move || {
            let list = match handy_recorder::list_input_devices() {
                Ok(devices) => JsDeviceList {
                    devices: devices.iter().map(JsInputDevice::from).collect(),
                    error: None,
                },
                Err(error) => JsDeviceList {
                    devices: Vec::new(),
                    error: Some(JsErrorInfo::from(&error)),
                },
            };
            deferred.resolve(move |_| Ok(list));
        })
        .map_err(|e| NapiError::from_reason(format!("cannot spawn a thread: {e}")))?;
    Ok(promise)
}

/// `"granted"`, `"denied"`, `"not-determined"`, or `"unknown"`.
#[napi(js_name = "permissionStatus")]
pub fn permission_status() -> &'static str {
    convert::permission(handy_recorder::permission_status())
}
