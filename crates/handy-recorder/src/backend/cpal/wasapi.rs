//! The WASAPI capture buffer: 500 ms of headroom when the device accepts it.

use std::sync::{Arc, Mutex};

use super::{DataCallback, DeviceFormat, ErrorCallback, build_any};

/// Builds the stream with a 500 ms capture buffer, which keeps bursty RDP
/// audio from overrunning, and falls back to the device's default buffer
/// when WASAPI rejects it. Some devices and Windows builds fail
/// `IAudioClient::Initialize` with `E_INVALIDARG` for a fixed buffer
/// duration on an event-driven shared-mode stream, Remote Audio included.
pub(super) fn build_with_headroom(
    device: &cpal::Device,
    format: DeviceFormat,
    config: cpal::StreamConfig,
    data: DataCallback,
    error: ErrorCallback,
) -> Result<cpal::Stream, cpal::Error> {
    // Callbacks still use the device period. Do not clamp to the reported
    // buffer range: Remote Audio reports only the device period, not the
    // shared-mode buffer capacity.
    let frames = config.sample_rate / 2;
    log::debug!("requesting {frames}-frame (500 ms) capture buffer from WASAPI");
    let mut fixed = config;
    fixed.buffer_size = cpal::BufferSize::Fixed(frames);

    let data = Arc::new(Mutex::new(Some(data)));
    let error = Arc::new(Mutex::new(Some(error)));
    let rejected = match build_any(device, format, fixed, lend_data(&data), lend_error(&error)) {
        Ok(stream) => return Ok(stream),
        Err(e) => e,
    };

    // A failed build has dropped the lent callbacks, so the slots are ours again.
    let (Some(data), Some(error)) = (reclaim(data), reclaim(error)) else {
        return Err(rejected);
    };
    log::warn!("WASAPI rejected the 500 ms capture buffer ({rejected}); using the default buffer");
    build_any(device, format, config, data, error)
}

/// Takes the callback back once nothing else holds the slot.
fn reclaim<T>(slot: Arc<Mutex<Option<T>>>) -> Option<T> {
    Arc::try_unwrap(slot).ok()?.into_inner().ok()?
}

/// Lends `slot`'s callback to a build that may fail. The audio thread takes
/// it from the slot on its first call, the only time it locks.
fn lend_data(slot: &Arc<Mutex<Option<DataCallback>>>) -> DataCallback {
    let slot = Arc::clone(slot);
    let mut data = None;
    Box::new(move |input| {
        if data.is_none() {
            data = slot.lock().ok().and_then(|mut s| s.take());
        }
        if let Some(data) = &mut data {
            data(input);
        }
    })
}

/// [`lend_data`] for the error callback.
fn lend_error(slot: &Arc<Mutex<Option<ErrorCallback>>>) -> ErrorCallback {
    let slot = Arc::clone(slot);
    let mut error = None;
    Box::new(move |e| {
        if error.is_none() {
            error = slot.lock().ok().and_then(|mut s| s.take());
        }
        if let Some(error) = &mut error {
            error(e);
        }
    })
}
