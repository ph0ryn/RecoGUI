mod microphone;

#[cfg(target_os = "macos")]
mod process_tap;

use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use cpal::Sample as _;
use rtrb::{Consumer, Producer, RingBuffer};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::application_core::{domain::AudioFrame, media::PcmNormalizer};

const RING_SECONDS: usize = 5;
const WORKER_READ_FRAMES: usize = 2_048;
const EVENT_CHANNEL_CAPACITY: usize = 64;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CaptureSource {
    Microphone { device_id: Option<String> },
    SystemAudio,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudioInputDevice {
    pub id: String,
    pub name: String,
    pub channels: u16,
    pub is_default: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CaptureEvent {
    Started { native_format: NativeFormat },
    Frame(AudioFrame),
    Error(AudioCaptureError),
    Ended,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AudioCaptureError {
    #[error("another audio capture is already active")]
    AlreadyActive,
    #[error("no microphone input device is available")]
    NoInputDevice,
    #[error("the selected microphone is no longer available: {0}")]
    DeviceNotFound(String),
    #[error("microphone access was denied")]
    MicrophonePermissionDenied,
    #[error("audio permission request timed out")]
    PermissionPromptTimedOut,
    #[cfg(not(target_os = "macos"))]
    #[error("desktop audio capture requires macOS 14.2 or newer")]
    UnsupportedOperatingSystem,
    #[error("desktop audio capture permission was denied")]
    SystemAudioPermissionDenied,
    #[error("RecoGUI could not be registered for exclusion from desktop audio capture")]
    SelfExclusionUnavailable,
    #[error("audio input overflowed its realtime buffer")]
    RingOverflow,
    #[error("audio device failed: {0}")]
    Device(String),
    #[error("Core Audio failed: {0}")]
    CoreAudio(String),
    #[error("invalid native audio format: {sample_rate} Hz, {channels} channels")]
    InvalidFormat { sample_rate: u32, channels: u16 },
    #[error("received {samples} samples that do not align to {channels} channels")]
    UnalignedSamples { samples: usize, channels: usize },
    #[error("audio resampling failed: {0}")]
    Resampler(String),
    #[error("audio capture thread terminated before startup")]
    StartupTerminated,
}

pub fn list_input_devices() -> Result<Vec<AudioInputDevice>, AudioCaptureError> {
    microphone::list_input_devices()
}

pub struct AudioCaptureManager {
    state: Mutex<ManagerState>,
}

#[derive(Clone)]
pub struct CaptureStartToken {
    stop: Arc<AtomicBool>,
    completion: Arc<CaptureCompletion>,
}

struct CaptureCompletion {
    done: Mutex<bool>,
    changed: Condvar,
}

impl CaptureCompletion {
    fn new() -> Self {
        Self {
            done: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    fn complete(&self) {
        *self.done.lock().expect("audio completion mutex poisoned") = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let mut done = self.done.lock().expect("audio completion mutex poisoned");
        while !*done {
            done = self
                .changed
                .wait(done)
                .expect("audio completion mutex poisoned");
        }
    }
}

impl Default for AudioCaptureManager {
    fn default() -> Self {
        Self {
            state: Mutex::new(ManagerState::Idle),
        }
    }
}

impl AudioCaptureManager {
    pub fn resolve_source(
        &self,
        source: &CaptureSource,
    ) -> Result<CaptureSource, AudioCaptureError> {
        match source {
            CaptureSource::Microphone { device_id } => {
                microphone::resolve_device_id(device_id.as_deref()).map(|device_id| {
                    CaptureSource::Microphone {
                        device_id: Some(device_id),
                    }
                })
            }
            CaptureSource::SystemAudio => {
                #[cfg(target_os = "macos")]
                {
                    process_tap::probe().map(|()| CaptureSource::SystemAudio)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    Err(AudioCaptureError::UnsupportedOperatingSystem)
                }
            }
        }
    }

    pub fn reserve_start(&self) -> Result<CaptureStartToken, AudioCaptureError> {
        let mut state = self.state.lock().expect("audio capture mutex poisoned");
        if !matches!(*state, ManagerState::Idle) {
            return Err(AudioCaptureError::AlreadyActive);
        }
        let token = CaptureStartToken {
            stop: Arc::new(AtomicBool::new(false)),
            completion: Arc::new(CaptureCompletion::new()),
        };
        *state = ManagerState::Starting {
            stop: token.stop.clone(),
            completion: token.completion.clone(),
        };
        Ok(token)
    }

    pub fn start_reserved(
        &self,
        token: CaptureStartToken,
        source: CaptureSource,
        start_sample: u64,
    ) -> Result<mpsc::Receiver<CaptureEvent>, AudioCaptureError> {
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        {
            let state = self.state.lock().expect("audio capture mutex poisoned");
            let ManagerState::Starting {
                stop, completion, ..
            } = &*state
            else {
                token.completion.complete();
                return Err(AudioCaptureError::StartupTerminated);
            };
            if !Arc::ptr_eq(stop, &token.stop)
                || !Arc::ptr_eq(completion, &token.completion)
                || token.stop.load(Ordering::Acquire)
            {
                token.completion.complete();
                return Err(AudioCaptureError::StartupTerminated);
            }
        }
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread_stop = token.stop.clone();
        let ready_for_error = ready_tx.clone();
        let thread_completion = token.completion.clone();
        if let Err(error) = thread::Builder::new()
            .name("reco-audio-source".into())
            .spawn(move || {
                let result = match source {
                    CaptureSource::Microphone { device_id } => microphone::run(
                        device_id.as_deref(),
                        start_sample,
                        thread_stop,
                        events_tx,
                        ready_tx,
                    ),
                    CaptureSource::SystemAudio => {
                        #[cfg(target_os = "macos")]
                        {
                            process_tap::run(start_sample, thread_stop, events_tx, ready_tx)
                        }
                        #[cfg(not(target_os = "macos"))]
                        {
                            let error = AudioCaptureError::UnsupportedOperatingSystem;
                            let _ = ready_tx.send(Err(error.clone()));
                            Err(error)
                        }
                    }
                };
                if let Err(error) = result {
                    let _ = ready_for_error.send(Err(error));
                }
                thread_completion.complete();
            })
        {
            token.completion.complete();
            self.finish_startup(&token);
            return Err(AudioCaptureError::Device(error.to_string()));
        }

        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            let result = ready_rx.recv_timeout(Duration::from_millis(100));
            if token.stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                self.transition_to_stopping(&token);
                return Err(AudioCaptureError::StartupTerminated);
            }
            match result {
                Ok(Ok(())) => {
                    let mut state = self.state.lock().expect("audio capture mutex poisoned");
                    let ManagerState::Starting { stop, completion } = &*state else {
                        return Err(AudioCaptureError::StartupTerminated);
                    };
                    if !Arc::ptr_eq(stop, &token.stop)
                        || !Arc::ptr_eq(completion, &token.completion)
                    {
                        return Err(AudioCaptureError::StartupTerminated);
                    }
                    *state = ManagerState::Active {
                        stop: token.stop.clone(),
                        completion: token.completion.clone(),
                    };
                    return Ok(events_rx);
                }
                Ok(Err(error)) => {
                    self.finish_startup(&token);
                    return Err(error);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.finish_startup(&token);
                    return Err(AudioCaptureError::StartupTerminated);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    pub fn cancel_reserved(&self, token: &CaptureStartToken) {
        token.stop.store(true, Ordering::Release);
        let mut state = self.state.lock().expect("audio capture mutex poisoned");
        let matches_reservation = matches!(
            &*state,
            ManagerState::Starting { stop, completion }
                if Arc::ptr_eq(stop, &token.stop)
                    && Arc::ptr_eq(completion, &token.completion)
        );
        if matches_reservation {
            token.completion.complete();
            *state = ManagerState::Idle;
        }
    }

    pub fn request_stop(&self) {
        let mut state = self.state.lock().expect("audio capture mutex poisoned");
        let replacement = match &*state {
            ManagerState::Idle | ManagerState::Stopping { .. } => None,
            ManagerState::Starting { stop, completion }
            | ManagerState::Active { stop, completion } => {
                stop.store(true, Ordering::Release);
                Some(ManagerState::Stopping {
                    completion: completion.clone(),
                })
            }
        };
        if let Some(replacement) = replacement {
            *state = replacement;
        }
    }

    pub fn wait_stopped(&self) {
        self.request_stop();
        let completion = {
            let state = self.state.lock().expect("audio capture mutex poisoned");
            match &*state {
                ManagerState::Idle => None,
                ManagerState::Starting { completion, .. }
                | ManagerState::Active { completion, .. }
                | ManagerState::Stopping { completion } => Some(completion.clone()),
            }
        };
        let Some(completion) = completion else {
            return;
        };
        completion.wait();
        let mut state = self.state.lock().expect("audio capture mutex poisoned");
        if state.has_completion(&completion) {
            *state = ManagerState::Idle;
        }
    }

    #[cfg(test)]
    fn is_active(&self) -> bool {
        !matches!(
            *self.state.lock().expect("audio capture mutex poisoned"),
            ManagerState::Idle
        )
    }

    fn finish_startup(&self, token: &CaptureStartToken) {
        self.transition_to_stopping(token);
        token.completion.wait();
        let mut state = self.state.lock().expect("audio capture mutex poisoned");
        if state.has_completion(&token.completion) {
            *state = ManagerState::Idle;
        }
    }

    fn transition_to_stopping(&self, token: &CaptureStartToken) {
        token.stop.store(true, Ordering::Release);
        let mut state = self.state.lock().expect("audio capture mutex poisoned");
        if state.has_completion(&token.completion) {
            *state = ManagerState::Stopping {
                completion: token.completion.clone(),
            };
        }
    }
}

impl Drop for AudioCaptureManager {
    fn drop(&mut self) {
        let state = self.state.get_mut().expect("audio capture mutex poisoned");
        let completion = match state {
            ManagerState::Idle => None,
            ManagerState::Starting { stop, completion }
            | ManagerState::Active { stop, completion } => {
                stop.store(true, Ordering::Release);
                Some(completion.clone())
            }
            ManagerState::Stopping { completion } => Some(completion.clone()),
        };
        if let Some(completion) = completion {
            completion.wait();
        }
    }
}

enum ManagerState {
    Idle,
    Starting {
        stop: Arc<AtomicBool>,
        completion: Arc<CaptureCompletion>,
    },
    Active {
        stop: Arc<AtomicBool>,
        completion: Arc<CaptureCompletion>,
    },
    Stopping {
        completion: Arc<CaptureCompletion>,
    },
}

impl ManagerState {
    fn has_completion(&self, target: &Arc<CaptureCompletion>) -> bool {
        match self {
            Self::Idle => false,
            Self::Starting { completion, .. }
            | Self::Active { completion, .. }
            | Self::Stopping { completion } => Arc::ptr_eq(completion, target),
        }
    }
}

pub(crate) struct RealtimeSink {
    producer: Producer<f32>,
    overflowed: Arc<AtomicBool>,
}

impl RealtimeSink {
    pub(crate) fn new(producer: Producer<f32>, overflowed: Arc<AtomicBool>) -> Self {
        Self {
            producer,
            overflowed,
        }
    }

    pub(crate) fn push(&mut self, samples: &[f32]) {
        if self.overflowed.load(Ordering::Relaxed) {
            return;
        }
        if self.producer.push_entire_slice(samples).is_err() {
            self.overflowed.store(true, Ordering::Release);
        }
    }

    pub(crate) fn available(&self) -> usize {
        self.producer.slots()
    }

    pub(crate) fn push_sample(&mut self, sample: f32) -> bool {
        if self.producer.push(sample).is_err() {
            self.overflowed.store(true, Ordering::Release);
            return false;
        }
        true
    }

    pub(crate) fn mark_overflow(&self) {
        self.overflowed.store(true, Ordering::Release);
    }

    pub(crate) fn push_converted<T>(&mut self, samples: &[T])
    where
        T: cpal::Sample,
        f32: cpal::FromSample<T>,
    {
        if self.overflowed.load(Ordering::Relaxed) {
            return;
        }
        if self.producer.slots() < samples.len() {
            self.overflowed.store(true, Ordering::Release);
            return;
        }
        for &sample in samples {
            if self.producer.push(f32::from_sample(sample)).is_err() {
                self.overflowed.store(true, Ordering::Release);
                return;
            }
        }
    }
}

pub(crate) fn create_ring(format: NativeFormat) -> (RealtimeSink, Consumer<f32>, Arc<AtomicBool>) {
    let capacity = format.sample_rate as usize * usize::from(format.channels) * RING_SECONDS;
    let (producer, consumer) = RingBuffer::new(capacity.max(1));
    let overflowed = Arc::new(AtomicBool::new(false));
    (
        RealtimeSink::new(producer, overflowed.clone()),
        consumer,
        overflowed,
    )
}

pub(crate) struct WorkerContext {
    pub(crate) format: NativeFormat,
    pub(crate) start_sample: u64,
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) source_stopped: Arc<AtomicBool>,
    pub(crate) overflowed: Arc<AtomicBool>,
    pub(crate) callback_failed: Arc<AtomicBool>,
    pub(crate) runtime_error: Arc<Mutex<Option<AudioCaptureError>>>,
    pub(crate) events: mpsc::Sender<CaptureEvent>,
}

pub(crate) fn run_worker(mut consumer: Consumer<f32>, context: WorkerContext) -> JoinHandle<()> {
    thread::Builder::new()
        .name("reco-audio-normalizer".into())
        .spawn(move || {
            let WorkerContext {
                format,
                start_sample,
                stop,
                source_stopped,
                overflowed,
                callback_failed,
                runtime_error,
                events,
            } = context;
            let mut normalizer =
                match PcmNormalizer::new(format.sample_rate, format.channels, start_sample) {
                    Ok(normalizer) => normalizer,
                    Err(error) => {
                        let _ = events.blocking_send(CaptureEvent::Error(
                            AudioCaptureError::Resampler(error.to_string()),
                        ));
                        stop.store(true, Ordering::Release);
                        return;
                    }
                };
            let channels = usize::from(format.channels);
            let mut scratch = vec![0.0; WORKER_READ_FRAMES * channels];

            loop {
                if overflowed.load(Ordering::Acquire) {
                    let _ =
                        events.blocking_send(CaptureEvent::Error(AudioCaptureError::RingOverflow));
                    stop.store(true, Ordering::Release);
                    return;
                }
                if callback_failed.load(Ordering::Acquire) {
                    let _ = events.blocking_send(CaptureEvent::Error(AudioCaptureError::Device(
                        "native audio callback failed".into(),
                    )));
                    stop.store(true, Ordering::Release);
                    return;
                }
                if let Some(error) = runtime_error
                    .lock()
                    .expect("audio error mutex poisoned")
                    .take()
                {
                    let _ = events.blocking_send(CaptureEvent::Error(error));
                    stop.store(true, Ordering::Release);
                    return;
                }

                let available = consumer.slots();
                let aligned = available.min(scratch.len()) / channels * channels;
                if aligned != 0 {
                    consumer
                        .pop_entire_slice(&mut scratch[..aligned])
                        .expect("available samples checked");
                    match normalizer.push_interleaved(&scratch[..aligned]) {
                        Ok(frames) => {
                            for frame in frames {
                                if events.blocking_send(CaptureEvent::Frame(frame)).is_err() {
                                    stop.store(true, Ordering::Release);
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            let _ = events.blocking_send(CaptureEvent::Error(
                                AudioCaptureError::Resampler(error.to_string()),
                            ));
                            stop.store(true, Ordering::Release);
                            return;
                        }
                    }
                    continue;
                }

                if source_stopped.load(Ordering::Acquire) {
                    if available != 0 {
                        let _ = events.blocking_send(CaptureEvent::Error(
                            AudioCaptureError::UnalignedSamples {
                                samples: available,
                                channels,
                            },
                        ));
                        return;
                    }
                    match normalizer.finish() {
                        Ok(frames) => {
                            for frame in frames {
                                if events.blocking_send(CaptureEvent::Frame(frame)).is_err() {
                                    return;
                                }
                            }
                            let _ = events.blocking_send(CaptureEvent::Ended);
                        }
                        Err(error) => {
                            let _ = events.blocking_send(CaptureEvent::Error(
                                AudioCaptureError::Resampler(error.to_string()),
                            ));
                        }
                    }
                    return;
                }
                thread::sleep(Duration::from_millis(2));
            }
        })
        .expect("failed to spawn audio normalization worker")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realtime_sink_reports_overflow_without_overwriting() {
        let (producer, mut consumer) = RingBuffer::new(2);
        let overflowed = Arc::new(AtomicBool::new(false));
        let mut sink = RealtimeSink::new(producer, overflowed.clone());
        sink.push(&[1.0, 2.0, 3.0]);
        assert!(overflowed.load(Ordering::Acquire));
        assert_eq!(consumer.pop(), Err(rtrb::PopError::Empty));
    }

    #[test]
    fn manager_rejects_parallel_capture_before_touching_devices() {
        let manager = AudioCaptureManager::default();
        let token = manager.reserve_start().unwrap();
        assert!(matches!(
            manager.reserve_start(),
            Err(AudioCaptureError::AlreadyActive)
        ));
        manager.request_stop();
        assert!(manager.is_active());
        assert!(matches!(
            manager.reserve_start(),
            Err(AudioCaptureError::AlreadyActive)
        ));
        token.completion.complete();
        manager.wait_stopped();
        assert!(!manager.is_active());
    }

    #[test]
    fn reserved_capture_cancelled_before_start_completes_stop() {
        let manager = AudioCaptureManager::default();
        let token = manager.reserve_start().unwrap();
        manager.request_stop();

        assert!(matches!(
            manager.start_reserved(token, CaptureSource::SystemAudio, 0),
            Err(AudioCaptureError::StartupTerminated)
        ));
        manager.wait_stopped();
        assert!(!manager.is_active());
    }

    #[test]
    fn concurrent_stop_waiters_observe_the_same_completion() {
        let manager = Arc::new(AudioCaptureManager::default());
        let token = manager.reserve_start().unwrap();
        manager.request_stop();
        let first = {
            let manager = manager.clone();
            thread::spawn(move || manager.wait_stopped())
        };
        let second = {
            let manager = manager.clone();
            thread::spawn(move || manager.wait_stopped())
        };

        token.completion.complete();
        first.join().unwrap();
        second.join().unwrap();
        assert!(!manager.is_active());
    }
}
