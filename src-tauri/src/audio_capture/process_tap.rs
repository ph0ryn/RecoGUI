use std::{
    cell::RefCell,
    ffi::{CStr, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::NonNull,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
    },
    thread,
    time::Duration,
};

use block2::RcBlock;
use objc2::{AnyThread, rc::Retained};
use objc2_core_audio::{
    AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData, AudioObjectID,
    AudioObjectPropertyAddress, CATapDescription, CATapMuteBehavior,
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey,
    kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioSubTapDriftCompensationKey,
    kAudioSubTapUIDKey, kAudioTapPropertyFormat,
};
use objc2_core_audio_types::{
    AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, kAudioFormatFlagIsBigEndian,
    kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagsNativeFloatPacked, kAudioFormatLinearPCM,
};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSObject, NSString};
use tokio::sync::mpsc;

use super::{
    AudioCaptureError, CaptureEvent, NativeFormat, RealtimeSink, WorkerContext, create_ring,
    run_worker,
};

const NO_ERR: i32 = 0;

pub(super) fn probe() -> Result<(), AudioCaptureError> {
    let prepared = unsafe { PreparedTap::new() }?;
    let (sink, _consumer, _overflowed) = create_ring(prepared.format);
    let tap = unsafe { prepared.start(sink, Arc::new(AtomicBool::new(false))) }?;
    thread::sleep(Duration::from_millis(50));
    drop(tap);
    Ok(())
}

pub(super) fn run(
    start_sample: u64,
    stop: Arc<AtomicBool>,
    events: mpsc::Sender<CaptureEvent>,
    ready: SyncSender<Result<(), AudioCaptureError>>,
) -> Result<(), AudioCaptureError> {
    let prepared = unsafe { PreparedTap::new() }?;
    let format = prepared.format;
    let (sink, consumer, overflowed) = create_ring(format);
    let runtime_error = Arc::new(Mutex::new(None));
    let callback_failed = Arc::new(AtomicBool::new(false));
    let tap = unsafe { prepared.start(sink, callback_failed.clone()) }?;

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
    drop(tap);
    source_stopped.store(true, Ordering::Release);
    let _ = worker.join();
    Ok(())
}

struct PreparedTap {
    tap_id: AudioObjectID,
    aggregate_id: AudioObjectID,
    description: Option<Retained<CATapDescription>>,
    format: NativeFormat,
}

impl PreparedTap {
    unsafe fn new() -> Result<Self, AudioCaptureError> {
        let process_id = translate_pid_to_audio_object(std::process::id() as i32)?;
        if process_id == 0 {
            return Err(AudioCaptureError::SelfExclusionUnavailable);
        }
        let processes =
            NSArray::from_retained_slice(&[NSNumber::numberWithUnsignedInt(process_id)]);
        let description = unsafe {
            CATapDescription::initStereoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &processes,
            )
        };
        unsafe {
            description.setName(&NSString::from_str("RecoGUI desktop audio"));
            description.setPrivate(true);
            description.setMuteBehavior(CATapMuteBehavior::Unmuted);
        }

        let mut tap_id = 0;
        let status = unsafe { AudioHardwareCreateProcessTap(Some(&description), &mut tap_id) };
        check_status("AudioHardwareCreateProcessTap", status)?;
        if tap_id == 0 {
            return Err(AudioCaptureError::CoreAudio(
                "AudioHardwareCreateProcessTap returned no tap".into(),
            ));
        }

        let format = match read_tap_format(tap_id) {
            Ok(format) => format,
            Err(error) => {
                unsafe {
                    let _ = AudioHardwareDestroyProcessTap(tap_id);
                }
                return Err(error);
            }
        };
        let tap_uid = unsafe { description.UUID().UUIDString() };
        let aggregate_id = match create_aggregate_device(&tap_uid) {
            Ok(id) => id,
            Err(error) => {
                unsafe {
                    let _ = AudioHardwareDestroyProcessTap(tap_id);
                }
                return Err(error);
            }
        };

        Ok(Self {
            tap_id,
            aggregate_id,
            description: Some(description),
            format,
        })
    }

    unsafe fn start(
        mut self,
        sink: RealtimeSink,
        callback_failed: Arc<AtomicBool>,
    ) -> Result<RunningTap, AudioCaptureError> {
        let stopped = Arc::new(AtomicBool::new(false));
        let callback_stopped = stopped.clone();
        let expected_channels = usize::from(self.format.channels);
        let sink = RefCell::new(sink);
        let block = RcBlock::new(
            move |_now: NonNull<AudioTimeStamp>,
                  input: NonNull<AudioBufferList>,
                  _input_time: NonNull<AudioTimeStamp>,
                  _output: NonNull<AudioBufferList>,
                  _output_time: NonNull<AudioTimeStamp>| {
                let callback_result = catch_unwind(AssertUnwindSafe(|| {
                    if callback_stopped.load(Ordering::Acquire) {
                        return;
                    }
                    if let Ok(mut sink) = sink.try_borrow_mut() {
                        if !unsafe {
                            copy_audio_buffer_list(input.as_ptr(), expected_channels, &mut sink)
                        } {
                            callback_failed.store(true, Ordering::Release);
                        }
                    } else {
                        callback_failed.store(true, Ordering::Release);
                    }
                }));
                if callback_result.is_err() {
                    callback_failed.store(true, Ordering::Release);
                }
            },
        );
        let mut io_proc_id: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcIDWithBlock(
                NonNull::from(&mut io_proc_id),
                self.aggregate_id,
                None,
                RcBlock::as_ptr(&block),
            )
        };
        check_status("AudioDeviceCreateIOProcIDWithBlock", status)?;
        if io_proc_id.is_none() {
            return Err(AudioCaptureError::CoreAudio(
                "AudioDeviceCreateIOProcIDWithBlock returned no IOProc".into(),
            ));
        }
        if let Err(error) = check_status("AudioDeviceStart", unsafe {
            AudioDeviceStart(self.aggregate_id, io_proc_id)
        }) {
            unsafe {
                let _ = AudioDeviceDestroyIOProcID(self.aggregate_id, io_proc_id);
            }
            return Err(error);
        }

        let running = RunningTap {
            aggregate_id: self.aggregate_id,
            io_proc_id,
            tap_id: self.tap_id,
            stopped,
            _block: block,
            _description: self.description.take().expect("description present"),
        };
        self.aggregate_id = 0;
        self.tap_id = 0;
        Ok(running)
    }
}

impl Drop for PreparedTap {
    fn drop(&mut self) {
        unsafe {
            if self.aggregate_id != 0 {
                let _ = AudioHardwareDestroyAggregateDevice(self.aggregate_id);
            }
            if self.tap_id != 0 {
                let _ = AudioHardwareDestroyProcessTap(self.tap_id);
            }
        }
    }
}

#[allow(clippy::type_complexity)]
struct RunningTap {
    aggregate_id: AudioObjectID,
    io_proc_id: AudioDeviceIOProcID,
    tap_id: AudioObjectID,
    stopped: Arc<AtomicBool>,
    _block: RcBlock<
        dyn Fn(
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
        ),
    >,
    _description: Retained<CATapDescription>,
}

impl Drop for RunningTap {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        unsafe {
            let _ = AudioDeviceStop(self.aggregate_id, self.io_proc_id);
            let _ = AudioDeviceDestroyIOProcID(self.aggregate_id, self.io_proc_id);
            let _ = AudioHardwareDestroyAggregateDevice(self.aggregate_id);
            let _ = AudioHardwareDestroyProcessTap(self.tap_id);
        }
    }
}

unsafe fn copy_audio_buffer_list(
    list: *const AudioBufferList,
    expected_channels: usize,
    sink: &mut RealtimeSink,
) -> bool {
    if list.is_null() {
        return false;
    }
    let count = unsafe { (*list).mNumberBuffers as usize };
    if count == 0 || expected_channels == 0 {
        return false;
    }
    let buffers = unsafe { std::slice::from_raw_parts((*list).mBuffers.as_ptr(), count) };
    let actual_channels = buffers
        .iter()
        .map(|buffer| buffer.mNumberChannels as usize)
        .sum::<usize>();
    if actual_channels != expected_channels
        || buffers
            .iter()
            .any(|buffer| buffer.mData.is_null() || buffer.mNumberChannels == 0)
    {
        return false;
    }

    let mut frame_count = usize::MAX;
    for buffer in buffers {
        let channels = buffer.mNumberChannels as usize;
        let bytes_per_frame = size_of::<f32>().saturating_mul(channels);
        let byte_count = buffer.mDataByteSize as usize;
        if !byte_count.is_multiple_of(bytes_per_frame) {
            return false;
        }
        frame_count = frame_count.min(byte_count / bytes_per_frame);
    }
    let total = frame_count.saturating_mul(actual_channels);
    if total == 0 {
        return true;
    }
    if sink.available() < total {
        sink.mark_overflow();
        return true;
    }
    if count == 1 {
        let samples = unsafe { std::slice::from_raw_parts(buffers[0].mData.cast::<f32>(), total) };
        sink.push(samples);
        return true;
    }
    for frame in 0..frame_count {
        for buffer in buffers {
            let channels = buffer.mNumberChannels as usize;
            let samples = unsafe {
                std::slice::from_raw_parts(
                    buffer.mData.cast::<f32>(),
                    frame_count.saturating_mul(channels),
                )
            };
            let start = frame.saturating_mul(channels);
            for &sample in &samples[start..start + channels] {
                if !sink.push_sample(sample) {
                    return true;
                }
            }
        }
    }
    true
}

fn translate_pid_to_audio_object(pid: i32) -> Result<AudioObjectID, AudioCaptureError> {
    let address = global_property(kAudioHardwarePropertyTranslatePIDToProcessObject);
    let mut output = 0;
    let mut size = size_of::<AudioObjectID>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&address),
            size_of::<i32>() as u32,
            (&pid as *const i32).cast::<c_void>(),
            NonNull::from(&mut size),
            NonNull::from(&mut output).cast(),
        )
    };
    check_status("translate RecoGUI PID to Core Audio process", status)?;
    Ok(output)
}

fn read_tap_format(tap_id: AudioObjectID) -> Result<NativeFormat, AudioCaptureError> {
    let address = global_property(kAudioTapPropertyFormat);
    let mut format = AudioStreamBasicDescription {
        mSampleRate: 0.0,
        mFormatID: 0,
        mFormatFlags: 0,
        mBytesPerPacket: 0,
        mFramesPerPacket: 0,
        mBytesPerFrame: 0,
        mChannelsPerFrame: 0,
        mBitsPerChannel: 0,
        mReserved: 0,
    };
    let mut size = size_of::<AudioStreamBasicDescription>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            tap_id,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut format).cast(),
        )
    };
    check_status("read process tap format", status)?;
    let native_float = format.mFormatFlags & kAudioFormatFlagsNativeFloatPacked
        == kAudioFormatFlagsNativeFloatPacked;
    let native_endian = format.mFormatFlags & kAudioFormatFlagIsBigEndian == 0;
    let bytes_per_frame = if format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 {
        size_of::<f32>() as u32
    } else {
        (size_of::<f32>() as u32).saturating_mul(format.mChannelsPerFrame)
    };
    if format.mFormatID != kAudioFormatLinearPCM
        || !native_float
        || !native_endian
        || format.mBitsPerChannel != (size_of::<f32>() * u8::BITS as usize) as u32
        || format.mBytesPerFrame != bytes_per_frame
    {
        return Err(AudioCaptureError::CoreAudio(
            "process tap did not provide native packed f32 PCM".into(),
        ));
    }
    let native = NativeFormat {
        sample_rate: format.mSampleRate.round() as u32,
        channels: format.mChannelsPerFrame as u16,
    };
    if native.sample_rate == 0 || native.channels == 0 {
        return Err(AudioCaptureError::InvalidFormat {
            sample_rate: native.sample_rate,
            channels: native.channels,
        });
    }
    Ok(native)
}

fn create_aggregate_device(tap_uid: &NSString) -> Result<AudioObjectID, AudioCaptureError> {
    let tap_entry: Retained<NSDictionary<NSString, NSObject>> = NSDictionary::from_slices::<NSString>(
        &[
            &key(kAudioSubTapUIDKey),
            &key(kAudioSubTapDriftCompensationKey),
        ],
        &[tap_uid, NSNumber::numberWithBool(true).as_ref()],
    );
    let taps: Retained<NSArray<NSObject>> =
        NSArray::from_retained_slice(&[Retained::into_super(tap_entry)]);
    let name = NSString::from_str("RecoGUI private desktop audio");
    let uid = NSString::from_str(&format!("com.ph0ryn.recogui.tap.{}", uuid::Uuid::new_v4()));
    let yes = NSNumber::numberWithBool(true);
    let no = NSNumber::numberWithBool(false);
    let dictionary: Retained<NSDictionary<NSString, NSObject>> =
        NSDictionary::from_slices::<NSString>(
            &[
                &key(kAudioAggregateDeviceNameKey),
                &key(kAudioAggregateDeviceUIDKey),
                &key(kAudioAggregateDeviceIsPrivateKey),
                &key(kAudioAggregateDeviceIsStackedKey),
                &key(kAudioAggregateDeviceTapAutoStartKey),
                &key(kAudioAggregateDeviceTapListKey),
            ],
            &[
                name.as_ref(),
                uid.as_ref(),
                yes.as_ref(),
                no.as_ref(),
                yes.as_ref(),
                taps.as_ref(),
            ],
        );
    let cf_dictionary = unsafe { &*(Retained::as_ptr(&dictionary) as *const CFDictionary) };
    let mut aggregate_id = 0;
    let status = unsafe {
        AudioHardwareCreateAggregateDevice(cf_dictionary, NonNull::from(&mut aggregate_id))
    };
    check_status("AudioHardwareCreateAggregateDevice", status)?;
    if aggregate_id == 0 {
        return Err(AudioCaptureError::CoreAudio(
            "AudioHardwareCreateAggregateDevice returned no device".into(),
        ));
    }
    Ok(aggregate_id)
}

fn global_property(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn key(value: &CStr) -> Retained<NSString> {
    NSString::from_str(value.to_str().unwrap_or_default())
}

fn check_status(context: &str, status: i32) -> Result<(), AudioCaptureError> {
    if status == NO_ERR {
        return Ok(());
    }
    const ILLEGAL_OPERATION: i32 = 0x6e6f7065;
    if status == ILLEGAL_OPERATION {
        return Err(AudioCaptureError::SystemAudioPermissionDenied);
    }
    let bytes = (status as u32).to_be_bytes();
    let detail = if bytes.iter().all(u8::is_ascii_graphic) {
        format!("OSStatus '{}' ({status})", String::from_utf8_lossy(&bytes))
    } else {
        format!("OSStatus {status}")
    };
    Err(AudioCaptureError::CoreAudio(format!("{context}: {detail}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping_preserves_permission_denial() {
        assert_eq!(
            check_status("tap", 0x6e6f7065),
            Err(AudioCaptureError::SystemAudioPermissionDenied)
        );
    }

    #[test]
    fn aggregate_key_conversion_is_lossless() {
        assert_eq!(
            key(kAudioAggregateDeviceNameKey).to_string(),
            kAudioAggregateDeviceNameKey.to_str().unwrap()
        );
    }
}
