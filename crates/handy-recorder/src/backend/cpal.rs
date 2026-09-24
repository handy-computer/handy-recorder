//! The CPAL 0.18 backend. Stream construction and preferred-format selection
//! are Handy's (`recorder.rs` at 8f9cf53c), moved behind the backend seam.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::{
    Backend, BackendError, BackendErrorKind, DataCallback, DeviceFormat, ErrorCallback,
    InputSample, InputStream, OpenDevice, SampleFormat,
};
use crate::InputDevice;

/// Preferred stream config cached per device. The two HAL property queries
/// in `get_preferred_config` cost ~40-85ms per open (worse on USB/Bluetooth),
/// which lands on the keypress->capture path in on-demand mode. Keyed by
/// device ID so a system-default change misses naturally; cleared whenever
/// an open fails so a stale rate/format self-heals on the caller's retry.
type ConfigCache = Arc<Mutex<Option<(cpal::DeviceId, cpal::SupportedStreamConfig)>>>;

pub(crate) struct CpalBackend {
    host: cpal::Host,
    config_cache: ConfigCache,
}

// One host for the process: the PulseAudio host opens a server connection
// when created. Both are thread-safe on every target this compiles for.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<cpal::Host>();
    assert_send_sync::<cpal::Device>();
};

impl CpalBackend {
    /// The process-wide backend. Sharing it also shares Handy's
    /// preferred-config cache across recorders, so reopening a device skips
    /// the HAL queries.
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
            config_cache: ConfigCache::default(),
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

/// Enumerates input devices with their identity. A device whose backend has
/// no ID gets one built from its name and occurrence ("USB Mic#1").
fn enumerate(host: &cpal::Host) -> Result<Vec<(InputDevice, cpal::Device)>, BackendError> {
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
            },
            device,
        ));
        *occurrence += 1;
    }
    Ok(out)
}

impl Backend for CpalBackend {
    fn list_input_devices(&self) -> Result<Vec<InputDevice>, BackendError> {
        Ok(enumerate(&self.host)?
            .into_iter()
            .map(|(info, _)| info)
            .collect())
    }

    fn open_device(&self, id: Option<&str>) -> Result<Box<dyn OpenDevice>, BackendError> {
        let host = &self.host;
        let (info, device) = match id {
            Some(id) => enumerate(host)?
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
                let info = enumerate(host)
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
                    });
                (info, device)
            }
        };

        let config_started = Instant::now();
        let device_id = device.id().ok();
        let cached_config = self
            .config_cache
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(id, _)| device_id.as_ref() == Some(id))
            .map(|(_, cfg)| *cfg);
        let config_was_cached = cached_config.is_some();
        let config = match cached_config {
            Some(cfg) => cfg,
            None => get_preferred_config(&device).map_err(|e| {
                self.clear_cache();
                map_error_during("Failed to fetch preferred config", e)
            })?,
        };
        log::debug!(
            "fetch_config={:?} (cached={config_was_cached})",
            config_started.elapsed()
        );

        let sample_format = match config.sample_format() {
            cpal::SampleFormat::U8 => SampleFormat::U8,
            cpal::SampleFormat::I8 => SampleFormat::I8,
            cpal::SampleFormat::I16 => SampleFormat::I16,
            cpal::SampleFormat::I32 => SampleFormat::I32,
            cpal::SampleFormat::F32 => SampleFormat::F32,
            sample_format => {
                self.clear_cache();
                return Err(BackendError::new(
                    BackendErrorKind::UnsupportedConfig,
                    format!("Unsupported sample format: {sample_format:?}"),
                ));
            }
        };

        Ok(Box::new(CpalOpenDevice {
            info,
            device,
            device_id,
            config,
            config_was_cached,
            format: DeviceFormat {
                sample_rate: config.sample_rate(),
                channels: config.channels(),
                sample_format,
            },
            config_cache: Arc::clone(&self.config_cache),
        }))
    }
}

impl CpalBackend {
    fn clear_cache(&self) {
        *self.config_cache.lock().unwrap() = None;
    }
}

struct CpalOpenDevice {
    info: InputDevice,
    device: cpal::Device,
    device_id: Option<cpal::DeviceId>,
    config: cpal::SupportedStreamConfig,
    config_was_cached: bool,
    format: DeviceFormat,
    config_cache: ConfigCache,
}

struct CpalStream(#[allow(dead_code)] cpal::Stream);

impl InputStream for CpalStream {}

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
        let result = (|| {
            let build_started = Instant::now();
            let stream = match self.format.sample_format {
                SampleFormat::U8 => build_stream::<u8>(&self.device, &self.config, data, error),
                SampleFormat::I8 => build_stream::<i8>(&self.device, &self.config, data, error),
                SampleFormat::I16 => build_stream::<i16>(&self.device, &self.config, data, error),
                SampleFormat::I32 => build_stream::<i32>(&self.device, &self.config, data, error),
                SampleFormat::F32 => build_stream::<f32>(&self.device, &self.config, data, error),
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
            Ok(stream)
        })();

        match result {
            Ok(stream) => {
                // The device accepted this config; remember it so the next
                // open skips the HAL property queries entirely.
                if !self.config_was_cached
                    && let Some(device_id) = self.device_id
                {
                    *self.config_cache.lock().unwrap() = Some((device_id, self.config));
                }
                Ok(Box::new(CpalStream(stream)))
            }
            Err(e) => {
                // A failed open may mean the cached config went stale
                // (device re-plugged, rate/format changed in the OS).
                // Drop it so the next attempt re-queries the device.
                *self.config_cache.lock().unwrap() = None;
                Err(e)
            }
        }
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
            BackendErrorKind::DeviceChanged => "The stream was rerouted to a new default device",
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

fn get_preferred_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, cpal::Error> {
    // Use the device's native/default sample rate and let the FrameResampler
    // downsample to 16kHz. This avoids forcing hardware into
    // a non-native rate which can cause issues on some devices (Bluetooth
    // codecs, certain ALSA drivers, etc.).
    let default_config = device.default_input_config()?;
    let target_rate = default_config.sample_rate();

    // Try to find the best sample format at the device's default rate
    let supported_configs = match device.supported_input_configs() {
        Ok(configs) => configs,
        Err(e) => {
            log::warn!("Could not enumerate input configs ({e}), using device default");
            return Ok(default_config);
        }
    };
    let mut best_config: Option<cpal::SupportedStreamConfigRange> = None;

    for config_range in supported_configs {
        if config_range.min_sample_rate() <= target_rate
            && config_range.max_sample_rate() >= target_rate
        {
            match best_config {
                None => best_config = Some(config_range),
                Some(ref current) => {
                    // Prioritize F32 > I16 > I32 > others
                    let score = |fmt: cpal::SampleFormat| match fmt {
                        cpal::SampleFormat::F32 => 4,
                        cpal::SampleFormat::I16 => 3,
                        cpal::SampleFormat::I32 => 2,
                        _ => 1,
                    };

                    if score(config_range.sample_format()) > score(current.sample_format()) {
                        best_config = Some(config_range);
                    }
                }
            }
        }
    }

    if let Some(config) = best_config {
        return Ok(config.with_sample_rate(target_rate));
    }

    // Fall back to device default if no config matched (exotic/virtual devices)
    log::warn!(
        "No supported config matched device default rate {:?}, using default config",
        target_rate
    );
    Ok(default_config)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    #[test]
    fn errors_the_stream_survives_are_classified_without_allocating() {
        for (cpal_kind, kind) in [
            (
                cpal::ErrorKind::DeviceChanged,
                BackendErrorKind::DeviceChanged,
            ),
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
