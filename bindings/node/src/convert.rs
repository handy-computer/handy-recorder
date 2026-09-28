//! Conversions between the library's types and plain JavaScript objects.

use handy_recorder::{Channels, Error, InputDevice, Permission, RecorderConfig, RecorderInfo};
use napi::{Error as NapiError, Result, Status};
use napi_derive::napi;

/// A library error as data. The wrapper turns it into a `RecorderError`.
#[napi(object, object_from_js = false)]
#[derive(Clone)]
pub struct JsErrorInfo {
    /// The `ErrorKind` variant's name, e.g. `"DeviceUnavailable"`.
    pub code: String,
    pub message: String,
    pub device: Option<JsInputDevice>,
    pub elapsed_ms: Option<f64>,
    pub detail: Option<String>,
}

impl From<&Error> for JsErrorInfo {
    fn from(error: &Error) -> Self {
        Self {
            // `ErrorKind` is non-exhaustive; its variant names are the codes,
            // so new kinds need no change here.
            code: format!("{:?}", error.kind()),
            message: error.to_string(),
            device: error.device().map(JsInputDevice::from),
            elapsed_ms: error.elapsed().map(|d| d.as_secs_f64() * 1000.0),
            detail: error.detail().map(str::to_owned),
        }
    }
}

#[napi(object, object_from_js = false)]
#[derive(Clone)]
pub struct JsInputDevice {
    pub id: String,
    pub name: String,
    pub occurrence: u32,
    pub backend: String,
    pub is_default: bool,
    pub id_is_stable: bool,
    pub channels: Option<u32>,
    pub is_monitor: bool,
}

impl From<&InputDevice> for JsInputDevice {
    fn from(device: &InputDevice) -> Self {
        Self {
            id: device.id.clone(),
            name: device.name.clone(),
            occurrence: device.occurrence,
            backend: device.backend.clone(),
            is_default: device.is_default,
            id_is_stable: device.id_is_stable,
            channels: device.channels.map(u32::from),
            is_monitor: device.is_monitor,
        }
    }
}

#[napi(object, object_from_js = false)]
pub struct JsFormat {
    pub sample_rate: u32,
    pub channels: u32,
}

#[napi(object, object_from_js = false)]
pub struct JsRecorderInfo {
    pub device: JsInputDevice,
    pub device_format: JsFormat,
    pub format: JsFormat,
    pub frames_per_chunk: u32,
}

impl From<&RecorderInfo> for JsRecorderInfo {
    fn from(info: &RecorderInfo) -> Self {
        let format = |f: handy_recorder::Format| JsFormat {
            sample_rate: f.sample_rate,
            channels: u32::from(f.channels),
        };
        Self {
            device: JsInputDevice::from(&info.device),
            device_format: format(info.device_format),
            format: format(info.format),
            frames_per_chunk: info.frames_per_chunk as u32,
        }
    }
}

/// `RecorderOptions` after the wrapper has validated its shape.
#[napi(object)]
pub struct JsConfig {
    pub device: Option<String>,
    pub sample_rate: Option<u32>,
    /// `"all"`, `"mono"`, or a zero-based channel index as a string.
    pub channels: Option<String>,
    pub frames_per_chunk: Option<u32>,
    pub take_headset: Option<bool>,
    /// Keep the whole recording for `stop`. Default true.
    pub collect: Option<bool>,
    /// Send each chunk to JavaScript. Default false.
    pub chunks: Option<bool>,
}

impl JsConfig {
    pub fn recorder_config(&self) -> Result<RecorderConfig> {
        let channels = match self.channels.as_deref() {
            None | Some("all") => Channels::All,
            Some("mono") => Channels::MixToMono,
            Some(index) => Channels::Only(index.parse().map_err(|_| {
                NapiError::new(
                    Status::InvalidArg,
                    format!(
                        "channels must be \"all\", \"mono\", or a channel index; got {index:?}"
                    ),
                )
            })?),
        };
        Ok(RecorderConfig {
            device: self.device.clone(),
            sample_rate: self.sample_rate,
            channels,
            frames_per_chunk: self.frames_per_chunk.map(|n| n as usize),
            take_headset: self.take_headset.unwrap_or(false),
        })
    }
}

pub fn permission(permission: Permission) -> &'static str {
    match permission {
        Permission::Granted => "granted",
        Permission::Denied => "denied",
        Permission::NotDetermined => "not-determined",
        _ => "unknown",
    }
}
