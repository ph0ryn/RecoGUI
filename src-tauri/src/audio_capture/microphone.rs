use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
    },
    thread,
    time::Duration,
};

use cpal::{
    Device, FromSample, I24, Sample, SampleFormat, Stream, StreamConfig, U24,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use tokio::sync::mpsc;

#[cfg(target_os = "macos")]
use block2::RcBlock;
#[cfg(target_os = "macos")]
use objc2::runtime::Bool;
#[cfg(target_os = "macos")]
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};

use super::{
    AudioCaptureError, AudioInputDevice, CaptureEvent, NativeFormat, RealtimeSink, WorkerContext,
    create_ring, run_worker,
};

pub(super) fn list_input_devices() -> Result<Vec<AudioInputDevice>, AudioCaptureError> {
    let host = cpal::default_host();
    let default_id = host
        .default_input_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let devices = host
        .input_devices()
        .map_err(|error| AudioCaptureError::Device(error.to_string()))?;
    let mut result = Vec::new();
    for device in devices {
        let id = device
            .id()
            .map_err(|error| AudioCaptureError::Device(error.to_string()))?
            .to_string();
        let name = device
            .description()
            .map_err(|error| AudioCaptureError::Device(error.to_string()))?
            .name()
            .to_owned();
        let channels = device
            .default_input_config()
            .map_err(|error| AudioCaptureError::Device(error.to_string()))?
            .channels();
        result.push(AudioInputDevice {
            is_default: default_id.as_deref() == Some(id.as_str()),
            id,
            name,
            channels,
        });
    }
    result.sort_by(|left, right| {
        right
            .is_default
            .cmp(&left.is_default)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(result)
}

pub(super) fn resolve_device_id(device_id: Option<&str>) -> Result<String, AudioCaptureError> {
    request_permission()?;
    let devices = list_input_devices()?;
    if devices.is_empty() {
        return Err(AudioCaptureError::NoInputDevice);
    }
    if let Some(device_id) = device_id {
        return devices
            .into_iter()
            .find(|device| device.id == device_id)
            .map(|device| device.id)
            .ok_or_else(|| AudioCaptureError::DeviceNotFound(device_id.to_owned()));
    }
    devices
        .iter()
        .find(|device| device.is_default)
        .or_else(|| devices.first())
        .map(|device| device.id.clone())
        .ok_or(AudioCaptureError::NoInputDevice)
}

#[cfg(target_os = "macos")]
fn request_permission() -> Result<(), AudioCaptureError> {
    let media_type = unsafe { AVMediaTypeAudio }
        .ok_or_else(|| AudioCaptureError::Device("AVMediaTypeAudio is unavailable".into()))?;
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) } {
        AVAuthorizationStatus::Authorized => Ok(()),
        AVAuthorizationStatus::Denied | AVAuthorizationStatus::Restricted => {
            Err(AudioCaptureError::MicrophonePermissionDenied)
        }
        AVAuthorizationStatus::NotDetermined => {
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            let completion = RcBlock::new(move |granted: Bool| {
                let _ = sender.send(granted.as_bool());
            });
            unsafe {
                AVCaptureDevice::requestAccessForMediaType_completionHandler(
                    media_type,
                    &completion,
                );
            }
            match receiver.recv_timeout(Duration::from_secs(120)) {
                Ok(true) => Ok(()),
                Ok(false) => Err(AudioCaptureError::MicrophonePermissionDenied),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    Err(AudioCaptureError::PermissionPromptTimedOut)
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    Err(AudioCaptureError::MicrophonePermissionDenied)
                }
            }
        }
        _ => Err(AudioCaptureError::MicrophonePermissionDenied),
    }
}

#[cfg(not(target_os = "macos"))]
fn request_permission() -> Result<(), AudioCaptureError> {
    Ok(())
}

pub(super) fn run(
    device_id: Option<&str>,
    start_sample: u64,
    stop: Arc<AtomicBool>,
    events: mpsc::Sender<CaptureEvent>,
    ready: SyncSender<Result<(), AudioCaptureError>>,
) -> Result<(), AudioCaptureError> {
    let host = cpal::default_host();
    let device = match device_id {
        Some(requested_id) => host
            .input_devices()
            .map_err(|error| AudioCaptureError::Device(error.to_string()))?
            .find(|device| device.id().is_ok_and(|id| id.to_string() == requested_id))
            .ok_or_else(|| AudioCaptureError::DeviceNotFound(requested_id.to_owned()))?,
        None => host
            .default_input_device()
            .ok_or(AudioCaptureError::NoInputDevice)?,
    };
    let supported = device
        .default_input_config()
        .map_err(|error| AudioCaptureError::Device(error.to_string()))?;
    let format = NativeFormat {
        sample_rate: supported.sample_rate(),
        channels: supported.channels(),
    };
    let stream_config = supported.config();
    let sample_format = supported.sample_format();
    let (sink, consumer, overflowed) = create_ring(format);
    let runtime_error = Arc::new(Mutex::new(None));
    let callback_failed = Arc::new(AtomicBool::new(false));
    let stream = build_stream(
        &device,
        &stream_config,
        sample_format,
        sink,
        runtime_error.clone(),
    )?;
    stream
        .play()
        .map_err(|error| AudioCaptureError::Device(error.to_string()))?;

    let source_stopped = Arc::new(AtomicBool::new(false));
    let worker = run_worker(
        consumer,
        WorkerContext {
            format,
            start_sample,
            stop: stop.clone(),
            source_stopped: source_stopped.clone(),
            overflowed,
            callback_failed,
            runtime_error,
            events: events.clone(),
        },
    );
    let _ = events.blocking_send(CaptureEvent::Started {
        native_format: format,
    });
    if ready.send(Ok(())).is_err() {
        stop.store(true, Ordering::Release);
    }

    while !stop.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(10));
    }
    drop(stream);
    source_stopped.store(true, Ordering::Release);
    let _ = worker.join();
    Ok(())
}

fn build_stream(
    device: &Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    sink: RealtimeSink,
    runtime_error: Arc<Mutex<Option<AudioCaptureError>>>,
) -> Result<Stream, AudioCaptureError> {
    macro_rules! build {
        ($sample:ty) => {{
            let mut sink = sink;
            let error_slot = runtime_error;
            device.build_input_stream(
                *config,
                move |samples: &[$sample], _| sink.push_converted(samples),
                move |error| {
                    *error_slot.lock().expect("audio error mutex poisoned") =
                        Some(AudioCaptureError::Device(error.to_string()));
                },
                None,
            )
        }};
    }

    let result = match sample_format {
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I24 => build!(I24),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I64 => build!(i64),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U24 => build!(U24),
        SampleFormat::U32 => build!(u32),
        SampleFormat::U64 => build!(u64),
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        other => {
            return Err(AudioCaptureError::Device(format!(
                "unsupported microphone sample format: {other}"
            )));
        }
    };
    result.map_err(|error| match error.kind() {
        cpal::ErrorKind::PermissionDenied => AudioCaptureError::MicrophonePermissionDenied,
        _ => AudioCaptureError::Device(error.to_string()),
    })
}

#[allow(dead_code)]
fn _sample_bounds<T>()
where
    T: Sample,
    f32: FromSample<T>,
{
}
