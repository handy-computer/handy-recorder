//! The CPAL 0.18 backend. Stream construction is Handy's (`recorder.rs` at
//! 8f9cf53c); the device opens at its OS default format (`get_preferred_config`).

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::Instant,
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::{
    Backend, BackendError, BackendErrorKind, DataCallback, DeviceFormat, ErrorCallback,
    InputSample, InputStream, OpenDevice, SampleFormat,
};
use crate::{InputDevice, Permission};

pub(crate) struct CpalBackend {
    host: cpal::Host,
}

// One host for the process: the PulseAudio host opens a server connection
// when created. Both are thread-safe on every target this compiles for.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<cpal::Host>();
    assert_send_sync::<cpal::Device>();
};

impl CpalBackend {
    /// The process-wide backend.
    // TODO(review): see TODO.md, "PulseAudio server restarts".
    pub fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<CpalBackend>> = OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| Arc::new(Self::new())))
    }

    /// Handy forces the ALSA host on Linux. The library targets the native
    /// PulseAudio host there (with ALSA fallback), which `default_host`
    /// selects when the `pulseaudio` feature is enabled and a server is
    /// running.
    pub fn new() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| "Unknown".into())
}

/// Whether a backend's device IDs survive restarts and replugs. ALSA PCM
/// names can move between cards, so they are best effort.
fn id_is_stable(host: cpal::HostId) -> bool {
    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
    if host == cpal::HostId::Alsa {
        return false;
    }
    let _ = host;
    true
}

/// Reads a device's channel count without opening it: the channel count of
/// the format it would be opened at (Handy's `preferred_input_channel_count`).
/// `None` on ALSA, where reading a device's configs opens its PCM, which
/// cpal avoids during enumeration because failed opens can leak descriptors.
fn channels_without_opening(host: cpal::HostId, device: &cpal::Device) -> Option<u16> {
    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
    if host == cpal::HostId::Alsa {
        return None;
    }
    let _ = host;
    get_preferred_config(device).ok().map(|c| c.channels())
}

/// Enumerates input devices with their identity. A device whose backend has
/// no ID gets one built from its name and occurrence ("USB Mic#1").
/// `with_channels` also reads each device's channel count, which costs a
/// config query per device on CoreAudio and WASAPI.
fn enumerate(
    host: &cpal::Host,
    with_channels: bool,
) -> Result<Vec<(InputDevice, cpal::Device)>, BackendError> {
    let default_id = host.default_input_device().and_then(|d| d.id().ok());
    let backend = host.id().name().to_owned();
    let mut occurrences = HashMap::<String, u32>::new();
    let mut out = Vec::new();
    for device in host.input_devices().map_err(map_error)? {
        let name = device_name(&device);
        let occurrence = occurrences.entry(name.clone()).or_default();
        let (id, stable, is_default) = match device.id() {
            Ok(id) => (
                id.to_string(),
                id_is_stable(id.host()),
                default_id.as_ref() == Some(&id),
            ),
            Err(_) => (format!("{name}#{occurrence}"), false, false),
        };
        out.push((
            InputDevice {
                id,
                name,
                occurrence: *occurrence,
                backend: backend.clone(),
                is_default,
                id_is_stable: stable,
                channels: if with_channels {
                    channels_without_opening(host.id(), &device)
                } else {
                    None
                },
            },
            device,
        ));
        *occurrence += 1;
    }
    Ok(out)
}

impl Backend for CpalBackend {
    fn permission_status(&self) -> Permission {
        crate::device::platform_permission_status()
    }

    fn list_input_devices(&self) -> Result<Vec<InputDevice>, BackendError> {
        Ok(enumerate(&self.host, true)?
            .into_iter()
            .map(|(info, _)| info)
            .collect())
    }

    fn open_device(&self, id: Option<&str>) -> Result<Box<dyn OpenDevice>, BackendError> {
        // TODO(review): see TODO.md, "Device enumeration on every open".
        let host = &self.host;
        let (mut info, device) = match id {
            Some(id) => enumerate(host, false)?
                .into_iter()
                .find(|(info, _)| info.id == id)
                .ok_or_else(|| {
                    BackendError::new(
                        BackendErrorKind::DeviceNotAvailable,
                        format!("No input device with ID {id:?}"),
                    )
                })?,
            None => {
                let device = host.default_input_device().ok_or_else(|| {
                    BackendError::new(
                        BackendErrorKind::DeviceNotAvailable,
                        "No input device found",
                    )
                })?;
                let id = device.id().ok();
                let info = enumerate(host, false)
                    .ok()
                    .and_then(|devices| {
                        devices
                            .into_iter()
                            .find(|(_, d)| id.is_some() && d.id().ok() == id)
                    })
                    .map(|(info, _)| info)
                    .unwrap_or_else(|| InputDevice {
                        id: id.as_ref().map_or_else(String::new, ToString::to_string),
                        name: device_name(&device),
                        occurrence: 0,
                        backend: host.id().name().to_owned(),
                        is_default: true,
                        id_is_stable: id.is_some_and(|id| id_is_stable(id.host())),
                        channels: None,
                    });
                (info, device)
            }
        };

        let config_started = Instant::now();
        let config = get_preferred_config(&device)
            .map_err(|e| map_error_during("Failed to fetch preferred config", e))?;
        log::debug!("fetch_config={:?}", config_started.elapsed());

        let sample_format = match config.sample_format() {
            cpal::SampleFormat::U8 => SampleFormat::U8,
            cpal::SampleFormat::I8 => SampleFormat::I8,
            cpal::SampleFormat::U16 => SampleFormat::U16,
            cpal::SampleFormat::I16 => SampleFormat::I16,
            cpal::SampleFormat::U24 => SampleFormat::U24,
            cpal::SampleFormat::I24 => SampleFormat::I24,
            cpal::SampleFormat::U32 => SampleFormat::U32,
            cpal::SampleFormat::I32 => SampleFormat::I32,
            cpal::SampleFormat::U64 => SampleFormat::U64,
            cpal::SampleFormat::I64 => SampleFormat::I64,
            cpal::SampleFormat::F32 => SampleFormat::F32,
            cpal::SampleFormat::F64 => SampleFormat::F64,
            sample_format => {
                return Err(BackendError::new(
                    BackendErrorKind::UnsupportedConfig,
                    format!("Unsupported sample format: {sample_format:?}"),
                ));
            }
        };

        // The opened device's channel count is the format it runs at.
        info.channels = Some(config.channels());

        Ok(Box::new(CpalOpenDevice {
            info,
            device_id: device.id().ok(),
            device,
            config,
            format: DeviceFormat {
                sample_rate: config.sample_rate(),
                channels: config.channels(),
                sample_format,
            },
        }))
    }
}

struct CpalOpenDevice {
    info: InputDevice,
    device: cpal::Device,
    device_id: Option<cpal::DeviceId>,
    config: cpal::SupportedStreamConfig,
    format: DeviceFormat,
}

struct CpalStream {
    _stream: cpal::Stream,
    /// The input device's ID, for finding its headset's output device.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    device_id: Option<cpal::DeviceId>,
    /// The silent output stream holding the headset (`take_headset`).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    headset: Option<cpal::Stream>,
}

impl InputStream for CpalStream {
    #[cfg(target_os = "macos")]
    fn hold_headset(&mut self, hold: bool) {
        if !hold {
            if self.headset.take().is_some() {
                log::debug!("released the headset");
            }
            return;
        }
        if self.headset.is_some() {
            return;
        }
        let Some(id) = &self.device_id else {
            return;
        };
        let Some(output_uid) = super::headset_macos::bluetooth_output_uid(id.id()) else {
            // Not a Bluetooth headset.
            return;
        };
        let started = Instant::now();
        match silent_output(&cpal::DeviceId::new(id.host(), &output_uid)) {
            Ok(stream) => {
                log::info!(
                    "holding the headset: silent output on {output_uid} (started in {:?})",
                    started.elapsed()
                );
                self.headset = Some(stream);
            }
            Err(e) => log::warn!("cannot hold the headset ({output_uid}): {e}"),
        }
    }
}

/// Starts a stream playing silence on the output device `id`.
#[cfg(target_os = "macos")]
fn silent_output(id: &cpal::DeviceId) -> Result<cpal::Stream, String> {
    let host = cpal::host_from_id(id.host()).map_err(|e| e.to_string())?;
    let device = host
        .device_by_id(id)
        .ok_or_else(|| "the output device is gone".to_owned())?;
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    let format = config.sample_format();
    // All-zero bytes are silence only for signed and float samples.
    if !matches!(
        format,
        cpal::SampleFormat::F32
            | cpal::SampleFormat::F64
            | cpal::SampleFormat::I8
            | cpal::SampleFormat::I16
            | cpal::SampleFormat::I32
    ) {
        return Err(format!("unsupported output sample format {format:?}"));
    }
    let stream = device
        .build_output_stream_raw(
            config.config(),
            format,
            |data: &mut cpal::Data, _: &cpal::OutputCallbackInfo| data.bytes_mut().fill(0),
            // Runs on the audio thread; the headset is best effort, and the
            // input stream reports anything that matters.
            |_| {},
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

impl OpenDevice for CpalOpenDevice {
    fn info(&self) -> &InputDevice {
        &self.info
    }

    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn start(
        self: Box<Self>,
        data: DataCallback,
        error: ErrorCallback,
    ) -> Result<Box<dyn InputStream>, BackendError> {
        let build_started = Instant::now();
        let stream = match self.format.sample_format {
            SampleFormat::U8 => build_stream::<u8>(&self.device, &self.config, data, error),
            SampleFormat::I8 => build_stream::<i8>(&self.device, &self.config, data, error),
            SampleFormat::U16 => build_stream::<u16>(&self.device, &self.config, data, error),
            SampleFormat::I16 => build_stream::<i16>(&self.device, &self.config, data, error),
            SampleFormat::U24 => build_stream::<cpal::U24>(&self.device, &self.config, data, error),
            SampleFormat::I24 => build_stream::<cpal::I24>(&self.device, &self.config, data, error),
            SampleFormat::U32 => build_stream::<u32>(&self.device, &self.config, data, error),
            SampleFormat::I32 => build_stream::<i32>(&self.device, &self.config, data, error),
            SampleFormat::U64 => build_stream::<u64>(&self.device, &self.config, data, error),
            SampleFormat::I64 => build_stream::<i64>(&self.device, &self.config, data, error),
            SampleFormat::F32 => build_stream::<f32>(&self.device, &self.config, data, error),
            SampleFormat::F64 => build_stream::<f64>(&self.device, &self.config, data, error),
        }
        .map_err(|e| map_error_during("Failed to build input stream", e))?;
        let build_elapsed = build_started.elapsed();

        let play_started = Instant::now();
        stream
            .play()
            .map_err(|e| map_error_during("Failed to start microphone stream", e))?;
        log::debug!(
            "build_stream={:?} play={:?}",
            build_elapsed,
            play_started.elapsed()
        );
        Ok(Box::new(CpalStream {
            _stream: stream,
            device_id: self.device_id,
            headset: None,
        }))
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    mut data: DataCallback,
    mut error: ErrorCallback,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample + InputSample,
{
    device.build_input_stream(
        config.config(),
        move |samples: &[T], _: &cpal::InputCallbackInfo| data(T::wrap(samples)),
        // May run on the platform audio thread. Converting allocates the
        // message; this happens only when the stream reports an error.
        move |e: cpal::Error| error(map_error(e)),
        None,
    )
}

/// Keeps Handy's message prefixes, which say which step failed (and which
/// Handy's `is_no_input_device_error` matches on).
fn map_error_during(step: &str, e: cpal::Error) -> BackendError {
    BackendError::new(map_kind(e.kind()), format!("{step}: {e}"))
}

fn map_error(e: cpal::Error) -> BackendError {
    let kind = map_kind(e.kind());
    if kind.stream_survives() {
        // May be reported repeatedly on the audio thread (xruns under load),
        // so it carries a fixed description instead of an allocated message.
        let message = match kind {
            BackendErrorKind::Xrun => "A buffer overrun or underrun occurred",
            _ => "Real-time scheduling was refused for the audio thread",
        };
        return BackendError::new(kind, message);
    }
    BackendError::new(kind, e.to_string())
}

fn map_kind(kind: cpal::ErrorKind) -> BackendErrorKind {
    match kind {
        cpal::ErrorKind::DeviceNotAvailable => BackendErrorKind::DeviceNotAvailable,
        cpal::ErrorKind::DeviceBusy => BackendErrorKind::DeviceBusy,
        cpal::ErrorKind::PermissionDenied => BackendErrorKind::PermissionDenied,
        cpal::ErrorKind::UnsupportedConfig => BackendErrorKind::UnsupportedConfig,
        cpal::ErrorKind::StreamInvalidated => BackendErrorKind::StreamInvalidated,
        cpal::ErrorKind::DeviceChanged => BackendErrorKind::DeviceChanged,
        cpal::ErrorKind::Xrun => BackendErrorKind::Xrun,
        cpal::ErrorKind::RealtimeDenied => BackendErrorKind::RealtimeDenied,
        _ => BackendErrorKind::Other,
    }
}

/// The format the OS has the device set to: the WASAPI mix format, the
/// CoreAudio stream format, or cpal's pick on ALSA. Nothing else is tried. A
/// format the OS reports as supported is not always safe: on Windows, some
/// drivers accept F32 and then deliver only zeros (Handy#2141), and on macOS a
/// listed format can differ from the stream format and fail to open (Handy#1163).
fn get_preferred_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, cpal::Error> {
    device.default_input_config()
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    #[test]
    fn errors_the_stream_survives_are_classified_without_allocating() {
        for (cpal_kind, kind) in [
            (cpal::ErrorKind::Xrun, BackendErrorKind::Xrun),
            (
                cpal::ErrorKind::RealtimeDenied,
                BackendErrorKind::RealtimeDenied,
            ),
        ] {
            let error = map_error(cpal::Error::with_message(cpal_kind, "detail"));
            assert_eq!(error.kind, kind);
            assert!(error.kind.stream_survives());
            assert!(matches!(error.message, Cow::Borrowed(_)));
        }
    }

    #[test]
    fn errors_that_end_the_stream_keep_the_platform_message() {
        for (cpal_kind, kind) in [
            (
                cpal::ErrorKind::DeviceNotAvailable,
                BackendErrorKind::DeviceNotAvailable,
            ),
            (
                cpal::ErrorKind::StreamInvalidated,
                BackendErrorKind::StreamInvalidated,
            ),
            (
                cpal::ErrorKind::DeviceChanged,
                BackendErrorKind::DeviceChanged,
            ),
            (
                cpal::ErrorKind::PermissionDenied,
                BackendErrorKind::PermissionDenied,
            ),
            (cpal::ErrorKind::BackendError, BackendErrorKind::Other),
        ] {
            let error = map_error(cpal::Error::with_message(cpal_kind, "AUDCLNT_E_X"));
            assert_eq!(error.kind, kind);
            assert!(!error.kind.stream_survives());
            assert_eq!(error.message, "AUDCLNT_E_X");
        }
        let error = map_error_during(
            "Failed to build input stream",
            cpal::Error::with_message(cpal::ErrorKind::PermissionDenied, "Unauthorized"),
        );
        assert_eq!(error.kind, BackendErrorKind::PermissionDenied);
        assert_eq!(error.message, "Failed to build input stream: Unauthorized");
    }

    /// Real hardware: lists input devices, then opens each by ID and reads
    /// its format. Run with `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs audio devices"]
    fn lists_and_resolves_real_devices() {
        let backend = CpalBackend::new();
        let devices = backend.list_input_devices().expect("list");
        assert!(!devices.is_empty(), "no input devices");
        assert!(devices.iter().filter(|d| d.is_default).count() <= 1);
        for device in &devices {
            let open = backend
                .open_device(Some(&device.id))
                .expect("resolve by id");
            eprintln!(
                "{} {} [{}] id={} stable={} {:?}",
                if device.is_default { "*" } else { " " },
                device.name,
                device.backend,
                device.id,
                device.id_is_stable,
                open.format()
            );
            assert_eq!(open.info(), device);
        }
        let default = backend.open_device(None).expect("default device");
        assert!(default.info().is_default);
    }
}
