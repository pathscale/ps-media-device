use std::{
    cell::Cell,
    ffi::c_void,
    fmt,
    marker::PhantomData,
    mem::{self, MaybeUninit, size_of},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{NonNull, null, null_mut},
    rc::Rc,
    slice,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use objc2_audio_toolbox::{
    AudioQueueAllocateBuffer, AudioQueueDispose, AudioQueueEnqueueBuffer, AudioQueueNewInput,
    AudioQueueNewOutput, AudioQueueRef, AudioQueueSetProperty, AudioQueueStart, AudioQueueStop,
    kAudioQueueProperty_CurrentDevice,
};
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, kAudioDevicePropertyDeviceUID, kAudioDevicePropertyStreams,
    kAudioHardwareNoError, kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
};
use objc2_core_audio_types::{
    AudioStreamBasicDescription, AudioTimeStamp, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM,
};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_foundation::{NSBundle, NSString};

const QUEUE_BUFFER_COUNT: usize = 3;
const MAX_CHANNELS: u32 = 8;
const MAX_BUFFER_FRAMES: u32 = 16_384;

std::thread_local! {
    static AUDIO_CALLBACK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

struct AudioCallbackScope;

impl AudioCallbackScope {
    fn enter() -> Self {
        AUDIO_CALLBACK_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self
    }
}

impl Drop for AudioCallbackScope {
    fn drop(&mut self) {
        AUDIO_CALLBACK_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

fn called_from_audio_callback() -> bool {
    AUDIO_CALLBACK_DEPTH.with(|depth| depth.get() != 0)
}

/// A CoreAudio device object ID. It is valid only for the current audio-server session;
/// use [`AudioDevice::uid`] when persisting a device choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AudioDeviceId(pub u32);

/// A discovered CoreAudio endpoint. Input and output support are reported separately because
/// many devices expose only one direction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    id: AudioDeviceId,
    uid: String,
    name: String,
    has_input: bool,
    has_output: bool,
}

impl AudioDevice {
    pub fn id(&self) -> AudioDeviceId {
        self.id
    }

    pub fn uid(&self) -> &str {
        &self.uid
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn has_input(&self) -> bool {
        self.has_input
    }

    pub fn has_output(&self) -> bool {
        self.has_output
    }
}

/// Interleaved 32-bit float stream format.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StreamConfig {
    pub sample_rate_hz: f64,
    pub channels: u32,
    pub frames_per_buffer: u32,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000.0,
            channels: 1,
            frames_per_buffer: 512,
        }
    }
}

impl StreamConfig {
    fn validate(self) -> Result<u32, AudioError> {
        if !self.sample_rate_hz.is_finite() || !(8_000.0..=192_000.0).contains(&self.sample_rate_hz)
        {
            return Err(AudioError::InvalidConfig(
                "sample rate must be finite and between 8000 and 192000 Hz",
            ));
        }
        if !(1..=MAX_CHANNELS).contains(&self.channels) {
            return Err(AudioError::InvalidConfig(
                "channels must be between 1 and 8",
            ));
        }
        if !(64..=MAX_BUFFER_FRAMES).contains(&self.frames_per_buffer) {
            return Err(AudioError::InvalidConfig(
                "frames_per_buffer must be between 64 and 16384",
            ));
        }

        self.frames_per_buffer
            .checked_mul(self.channels)
            .and_then(|samples| samples.checked_mul(size_of::<f32>() as u32))
            .ok_or(AudioError::InvalidConfig("audio buffer size overflowed"))
    }

    fn asbd(self) -> AudioStreamBasicDescription {
        let bytes_per_frame = self.channels * size_of::<f32>() as u32;
        AudioStreamBasicDescription {
            mSampleRate: self.sample_rate_hz,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: bytes_per_frame,
            mFramesPerPacket: 1,
            mBytesPerFrame: bytes_per_frame,
            mChannelsPerFrame: self.channels,
            mBitsPerChannel: (size_of::<f32>() * 8) as u32,
            mReserved: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    CoreAudioStatus {
        operation: &'static str,
        status: i32,
    },
    InputMediaTypeUnavailable,
    InputPermissionNotAuthorized,
    MissingMicrophoneUsageDescription,
    DeviceNotFound,
    UnsupportedDirection(&'static str),
    InvalidConfig(&'static str),
    InvalidInputBuffer,
    CallbackPanicked,
    CalledFromAudioCallback,
    CleanupFailed {
        setup: Box<AudioError>,
        cleanup: Box<AudioError>,
    },
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CoreAudioStatus { operation, status } => {
                write!(f, "CoreAudio {operation} failed with OSStatus {status}")
            }
            Self::InputMediaTypeUnavailable => {
                f.write_str("AVFoundation's audio media type is unavailable")
            }
            Self::InputPermissionNotAuthorized => {
                f.write_str("microphone permission has not already been granted")
            }
            Self::MissingMicrophoneUsageDescription => f.write_str(
                "the host bundle must provide NSMicrophoneUsageDescription before capture",
            ),
            Self::DeviceNotFound => f.write_str("audio device is no longer available"),
            Self::UnsupportedDirection(direction) => {
                write!(f, "selected audio device does not support {direction}")
            }
            Self::InvalidConfig(message) => f.write_str(message),
            Self::InvalidInputBuffer => {
                f.write_str("audio input buffer has an invalid byte layout")
            }
            Self::CallbackPanicked => f.write_str("audio callback panicked and was disabled"),
            Self::CalledFromAudioCallback => {
                f.write_str("opening an audio stream from an audio callback is not supported")
            }
            Self::CleanupFailed { setup, cleanup } => {
                write!(f, "{setup}; additionally, {cleanup}")
            }
        }
    }
}

impl std::error::Error for AudioError {}

/// A running capture stream. The handle is intentionally `!Send` and `!Sync`: its ownership
/// stays on the control thread so safe code cannot destroy the queue from its own callback.
/// The callback receives interleaved `f32` samples for each queue buffer.
pub struct AudioInput {
    queue: AudioQueueRef,
    context: *mut InputContext,
    _thread_affinity: PhantomData<Rc<()>>,
}

/// A running playback stream. The handle is intentionally `!Send` and `!Sync` so safe code
/// cannot destroy the queue from its own callback. The callback must fill the complete
/// interleaved `f32` sample buffer each time.
pub struct AudioOutput {
    queue: AudioQueueRef,
    context: *mut OutputContext,
    _thread_affinity: PhantomData<Rc<()>>,
}

type SampleInputCallback = Box<dyn FnMut(&[f32]) + Send>;
type SampleOutputCallback = Box<dyn FnMut(&mut [f32]) + Send>;
type ErrorCallback = Box<dyn FnMut(AudioError) + Send>;

struct InputContext {
    stopping: AtomicBool,
    sample_callback: Mutex<Option<SampleInputCallback>>,
    error_callback: Mutex<Option<ErrorCallback>>,
}

struct OutputContext {
    stopping: AtomicBool,
    sample_callback: Mutex<Option<SampleOutputCallback>>,
    error_callback: Mutex<Option<ErrorCallback>>,
}

trait QueueContext {
    fn stopping(&self) -> &AtomicBool;

    fn begin_stopping(&self) {
        self.stopping().store(true, Ordering::Release);
    }

    fn is_stopping(&self) -> bool {
        self.stopping().load(Ordering::Acquire)
    }
}

impl QueueContext for InputContext {
    fn stopping(&self) -> &AtomicBool {
        &self.stopping
    }
}

impl QueueContext for OutputContext {
    fn stopping(&self) -> &AtomicBool {
        &self.stopping
    }
}

impl AudioInput {
    /// Stop immediately and release the CoreAudio queue.
    pub fn stop(mut self) -> Result<(), AudioError> {
        self.close()
    }

    fn close(&mut self) -> Result<(), AudioError> {
        close_queue(&mut self.queue, &mut self.context)
    }
}

impl AudioOutput {
    /// Stop immediately and release the CoreAudio queue.
    pub fn stop(mut self) -> Result<(), AudioError> {
        self.close()
    }

    fn close(&mut self) -> Result<(), AudioError> {
        close_queue(&mut self.queue, &mut self.context)
    }
}

impl Drop for AudioInput {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Enumerate audio devices and their input/output capabilities.
pub fn devices() -> Result<Vec<AudioDevice>, AudioError> {
    let ids = property_array::<AudioObjectID>(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
    )?;

    ids.into_iter()
        .map(|id| {
            Ok(AudioDevice {
                id: AudioDeviceId(id),
                uid: string_property(id, kAudioDevicePropertyDeviceUID)?,
                name: string_property(id, kAudioObjectPropertyName)?,
                has_input: device_has_streams(id, kAudioObjectPropertyScopeInput),
                has_output: device_has_streams(id, kAudioObjectPropertyScopeOutput),
            })
        })
        .collect()
}

/// Return the current system default input device.
pub fn default_input_device() -> Result<AudioDevice, AudioError> {
    default_device(kAudioHardwarePropertyDefaultInputDevice)
}

/// Return the current system default output device.
pub fn default_output_device() -> Result<AudioDevice, AudioError> {
    default_device(kAudioHardwarePropertyDefaultOutputDevice)
}

/// Check that microphone capture is already authorized and the host bundle has a nonempty
/// `NSMicrophoneUsageDescription`. This is read-only and never requests permission.
pub fn preflight_input_permission() -> Result<(), AudioError> {
    let media_type = unsafe { AVMediaTypeAudio }.ok_or(AudioError::InputMediaTypeUnavailable)?;
    let authorization = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };
    if authorization != AVAuthorizationStatus::Authorized {
        return Err(AudioError::InputPermissionNotAuthorized);
    }

    let usage_description_key = NSString::from_str("NSMicrophoneUsageDescription");
    let has_usage_description = NSBundle::mainBundle()
        .objectForInfoDictionaryKey(&usage_description_key)
        .and_then(|value| value.downcast::<NSString>().ok())
        .is_some_and(|value| !value.to_string().trim().is_empty());
    if !has_usage_description {
        return Err(AudioError::MissingMicrophoneUsageDescription);
    }

    Ok(())
}

/// Start capturing interleaved `f32` samples from `device`.
///
/// The host application must include `NSMicrophoneUsageDescription` in its app bundle. Audio
/// callbacks run on a CoreAudio-owned thread; keep them short and non-blocking. If the sample
/// callback panics, it is disabled and the error callback is notified.
pub fn start_input(
    device: &AudioDevice,
    config: StreamConfig,
    sample_callback: impl FnMut(&[f32]) + Send + 'static,
    error_callback: impl FnMut(AudioError) + Send + 'static,
) -> Result<AudioInput, AudioError> {
    if called_from_audio_callback() {
        return Err(AudioError::CalledFromAudioCallback);
    }
    if !device.has_input {
        return Err(AudioError::UnsupportedDirection("input"));
    }
    preflight_input_permission()?;
    let buffer_bytes = config.validate()?;
    let mut format = config.asbd();
    let context = Box::into_raw(Box::new(InputContext {
        stopping: AtomicBool::new(false),
        sample_callback: Mutex::new(Some(Box::new(sample_callback))),
        error_callback: Mutex::new(Some(Box::new(error_callback))),
    }));
    let mut queue: AudioQueueRef = null_mut();

    let status = unsafe {
        AudioQueueNewInput(
            NonNull::from(&mut format),
            Some(input_callback),
            context.cast(),
            None,
            None,
            0,
            NonNull::from(&mut queue),
        )
    };
    if status != kAudioHardwareNoError {
        return Err(cleanup_after_setup_failure(
            queue,
            context,
            AudioError::CoreAudioStatus {
                operation: "AudioQueueNewInput",
                status,
            },
        ));
    }

    if let Err(error) = set_queue_device(queue, device) {
        return Err(cleanup_after_setup_failure(queue, context, error));
    }

    for _ in 0..QUEUE_BUFFER_COUNT {
        let mut buffer = null_mut();
        let status =
            unsafe { AudioQueueAllocateBuffer(queue, buffer_bytes, NonNull::from(&mut buffer)) };
        if let Err(error) = check_status("AudioQueueAllocateBuffer", status) {
            return Err(cleanup_after_setup_failure(queue, context, error));
        }
        let status = unsafe { AudioQueueEnqueueBuffer(queue, buffer, 0, null()) };
        if let Err(error) = check_status("AudioQueueEnqueueBuffer", status) {
            return Err(cleanup_after_setup_failure(queue, context, error));
        }
    }

    let status = unsafe { AudioQueueStart(queue, null::<AudioTimeStamp>()) };
    if let Err(error) = check_status("AudioQueueStart", status) {
        return Err(cleanup_after_setup_failure(queue, context, error));
    }

    Ok(AudioInput {
        queue,
        context,
        _thread_affinity: PhantomData,
    })
}

/// Start playing interleaved `f32` samples to `device`.
///
/// The callback runs on a CoreAudio-owned thread and must fill the entire supplied slice on
/// each call. It should be short and non-blocking. If it panics, it is disabled; playback
/// continues with silence and the error callback is notified.
pub fn start_output(
    device: &AudioDevice,
    config: StreamConfig,
    sample_callback: impl FnMut(&mut [f32]) + Send + 'static,
    error_callback: impl FnMut(AudioError) + Send + 'static,
) -> Result<AudioOutput, AudioError> {
    if called_from_audio_callback() {
        return Err(AudioError::CalledFromAudioCallback);
    }
    if !device.has_output {
        return Err(AudioError::UnsupportedDirection("output"));
    }
    let buffer_bytes = config.validate()?;
    let mut format = config.asbd();
    let context = Box::into_raw(Box::new(OutputContext {
        stopping: AtomicBool::new(false),
        sample_callback: Mutex::new(Some(Box::new(sample_callback))),
        error_callback: Mutex::new(Some(Box::new(error_callback))),
    }));
    let mut queue: AudioQueueRef = null_mut();

    let status = unsafe {
        AudioQueueNewOutput(
            NonNull::from(&mut format),
            Some(output_callback),
            context.cast(),
            None,
            None,
            0,
            NonNull::from(&mut queue),
        )
    };
    if status != kAudioHardwareNoError {
        return Err(cleanup_after_setup_failure(
            queue,
            context,
            AudioError::CoreAudioStatus {
                operation: "AudioQueueNewOutput",
                status,
            },
        ));
    }

    if let Err(error) = set_queue_device(queue, device) {
        return Err(cleanup_after_setup_failure(queue, context, error));
    }

    for _ in 0..QUEUE_BUFFER_COUNT {
        let mut buffer = null_mut();
        let status =
            unsafe { AudioQueueAllocateBuffer(queue, buffer_bytes, NonNull::from(&mut buffer)) };
        if let Err(error) = check_status("AudioQueueAllocateBuffer", status) {
            return Err(cleanup_after_setup_failure(queue, context, error));
        }
        unsafe { render_output(context, buffer) };
        let status = unsafe { AudioQueueEnqueueBuffer(queue, buffer, 0, null()) };
        if let Err(error) = check_status("AudioQueueEnqueueBuffer", status) {
            return Err(cleanup_after_setup_failure(queue, context, error));
        }
    }

    let status = unsafe { AudioQueueStart(queue, null::<AudioTimeStamp>()) };
    if let Err(error) = check_status("AudioQueueStart", status) {
        return Err(cleanup_after_setup_failure(queue, context, error));
    }

    Ok(AudioOutput {
        queue,
        context,
        _thread_affinity: PhantomData,
    })
}

fn default_device(selector: u32) -> Result<AudioDevice, AudioError> {
    let id: AudioObjectID = property_value(
        kAudioObjectSystemObject as AudioObjectID,
        selector,
        kAudioObjectPropertyScopeGlobal,
    )?;
    devices()?
        .into_iter()
        .find(|device| device.id.0 == id)
        .ok_or(AudioError::DeviceNotFound)
}

fn device_has_streams(device_id: AudioObjectID, scope: u32) -> bool {
    property_data_size(device_id, kAudioDevicePropertyStreams, scope)
        .is_ok_and(|size| size >= size_of::<AudioObjectID>() as u32)
}

fn string_property(object_id: AudioObjectID, selector: u32) -> Result<String, AudioError> {
    let raw: *mut CFString = property_value(object_id, selector, kAudioObjectPropertyScopeGlobal)?;
    let raw = NonNull::new(raw).ok_or(AudioError::DeviceNotFound)?;
    // CoreAudio returns these CFString properties with a +1 retain count.
    let string = unsafe { CFRetained::from_raw(raw) };
    Ok(string.to_string())
}

fn property_array<T>(
    object_id: AudioObjectID,
    selector: u32,
    scope: u32,
) -> Result<Vec<T>, AudioError>
where
    T: Copy,
{
    let address = property_address(selector, scope);
    let mut byte_size = 0u32;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object_id,
            NonNull::from(&address),
            0,
            null(),
            NonNull::from(&mut byte_size),
        )
    };
    check_status("AudioObjectGetPropertyDataSize", status)?;
    if byte_size == 0 {
        return Ok(Vec::new());
    }
    let element_size = size_of::<T>();
    if element_size == 0 || byte_size as usize % element_size != 0 {
        return Err(AudioError::InvalidConfig(
            "CoreAudio returned a malformed property array",
        ));
    }
    let mut values = Vec::<T>::with_capacity(byte_size as usize / element_size);
    let allocated_size = byte_size;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object_id,
            NonNull::from(&address),
            0,
            null(),
            NonNull::from(&mut byte_size),
            NonNull::new(values.as_mut_ptr().cast::<c_void>()).unwrap(),
        )
    };
    check_status("AudioObjectGetPropertyData", status)?;
    if byte_size > allocated_size || byte_size as usize % element_size != 0 {
        return Err(AudioError::InvalidConfig(
            "CoreAudio property changed while it was being read",
        ));
    }
    // The API succeeded and reported a byte count no larger than the allocated capacity.
    unsafe { values.set_len(byte_size as usize / element_size) };
    Ok(values)
}

fn property_value<T>(object_id: AudioObjectID, selector: u32, scope: u32) -> Result<T, AudioError> {
    let address = property_address(selector, scope);
    let mut data_size = size_of::<T>() as u32;
    let mut value = MaybeUninit::<T>::uninit();
    let status = unsafe {
        AudioObjectGetPropertyData(
            object_id,
            NonNull::from(&address),
            0,
            null(),
            NonNull::from(&mut data_size),
            NonNull::new(value.as_mut_ptr().cast::<c_void>()).unwrap(),
        )
    };
    check_status("AudioObjectGetPropertyData", status)?;
    if data_size as usize != size_of::<T>() {
        return Err(AudioError::InvalidConfig(
            "CoreAudio returned an unexpected property size",
        ));
    }
    Ok(unsafe { value.assume_init() })
}

fn property_data_size(
    object_id: AudioObjectID,
    selector: u32,
    scope: u32,
) -> Result<u32, AudioError> {
    let address = property_address(selector, scope);
    let mut byte_size = 0u32;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object_id,
            NonNull::from(&address),
            0,
            null(),
            NonNull::from(&mut byte_size),
        )
    };
    check_status("AudioObjectGetPropertyDataSize", status)?;
    Ok(byte_size)
}

fn property_address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn set_queue_device(queue: AudioQueueRef, device: &AudioDevice) -> Result<(), AudioError> {
    let uid = CFString::from_str(&device.uid);
    let mut uid_ref: *const CFString = &*uid;
    let status = unsafe {
        AudioQueueSetProperty(
            queue,
            kAudioQueueProperty_CurrentDevice,
            NonNull::from(&mut uid_ref).cast(),
            size_of::<*const CFString>() as u32,
        )
    };
    check_status("AudioQueueSetProperty(CurrentDevice)", status)
}

fn close_queue<T: QueueContext>(
    queue: &mut AudioQueueRef,
    context: &mut *mut T,
) -> Result<(), AudioError> {
    let queue_value = mem::replace(queue, null_mut());
    let context_value = mem::replace(context, null_mut());
    dispose_queue(queue_value, context_value)
}

fn cleanup_after_setup_failure<T: QueueContext>(
    queue: AudioQueueRef,
    context: *mut T,
    setup_error: AudioError,
) -> AudioError {
    match dispose_queue(queue, context) {
        Ok(()) => setup_error,
        Err(cleanup_error) => AudioError::CleanupFailed {
            setup: Box::new(setup_error),
            cleanup: Box::new(cleanup_error),
        },
    }
}

fn dispose_queue<T: QueueContext>(queue: AudioQueueRef, context: *mut T) -> Result<(), AudioError> {
    if !context.is_null() {
        unsafe { (&*context).begin_stopping() };
    }

    if queue.is_null() {
        if !context.is_null() {
            unsafe { drop(Box::from_raw(context)) };
        }
        return Ok(());
    }

    let stop_status = unsafe { AudioQueueStop(queue, true) };
    let dispose_status = unsafe { AudioQueueDispose(queue, true) };

    if dispose_status == kAudioHardwareNoError {
        // A successful dispose guarantees the queue can no longer call user_data.
        if !context.is_null() {
            unsafe { drop(Box::from_raw(context)) };
        }
    } else {
        // CoreAudio may still own a callback into user_data. Intentionally retain the
        // allocation rather than risk freeing it while a callback can still run.
    }

    // Prioritize a failed dispose because it means the callback context was retained.
    check_status("AudioQueueDispose", dispose_status)?;
    check_status("AudioQueueStop", stop_status)
}

fn check_status(operation: &'static str, status: i32) -> Result<(), AudioError> {
    if status == kAudioHardwareNoError {
        Ok(())
    } else {
        Err(AudioError::CoreAudioStatus { operation, status })
    }
}

unsafe extern "C-unwind" fn input_callback(
    user_data: *mut c_void,
    queue: AudioQueueRef,
    buffer: *mut objc2_audio_toolbox::AudioQueueBuffer,
    _start_time: NonNull<AudioTimeStamp>,
    _packet_count: u32,
    _packet_descriptions: *const objc2_core_audio_types::AudioStreamPacketDescription,
) {
    if user_data.is_null() || buffer.is_null() {
        return;
    }
    let _scope = AudioCallbackScope::enter();
    let context = unsafe { &*(user_data.cast::<InputContext>()) };
    if context.is_stopping() {
        return;
    }
    let audio_buffer = unsafe { &mut *buffer };
    let byte_size = audio_buffer.mAudioDataByteSize as usize;
    let data = audio_buffer.mAudioData.as_ptr().cast::<f32>();
    // The binding supplies a NonNull pointer, but valid byte length and f32
    // alignment must still hold before constructing a Rust slice.
    if byte_size > audio_buffer.mAudioDataBytesCapacity as usize
        || byte_size % size_of::<f32>() != 0
        || data.addr() % std::mem::align_of::<f32>() != 0
    {
        report_input_error(context, AudioError::InvalidInputBuffer);
        return;
    }
    if byte_size % size_of::<f32>() == 0 {
        let samples = unsafe { slice::from_raw_parts(data, byte_size / size_of::<f32>()) };
        let mut callback_guard = context
            .sample_callback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let callback_panicked = callback_guard.as_mut().is_some_and(|callback_fn| {
            catch_unwind(AssertUnwindSafe(|| callback_fn(samples))).is_err()
        });
        if callback_panicked {
            *callback_guard = None;
            drop(callback_guard);
            report_input_error(context, AudioError::CallbackPanicked);
        }
    }

    if context.is_stopping() {
        return;
    }
    let status = unsafe { AudioQueueEnqueueBuffer(queue, buffer, 0, null()) };
    if let Err(error) = check_status("AudioQueueEnqueueBuffer(input callback)", status) {
        if !context.is_stopping() {
            report_input_error(context, error);
        }
    }
}

unsafe extern "C-unwind" fn output_callback(
    user_data: *mut c_void,
    queue: AudioQueueRef,
    buffer: *mut objc2_audio_toolbox::AudioQueueBuffer,
) {
    if user_data.is_null() || buffer.is_null() {
        return;
    }
    let _scope = AudioCallbackScope::enter();
    let context = user_data.cast::<OutputContext>();
    let context_ref = unsafe { &*context };
    if context_ref.is_stopping() {
        return;
    }
    unsafe { render_output(context, buffer) };
    if context_ref.is_stopping() {
        return;
    }
    let status = unsafe { AudioQueueEnqueueBuffer(queue, buffer, 0, null()) };
    if let Err(error) = check_status("AudioQueueEnqueueBuffer(output callback)", status) {
        if !context_ref.is_stopping() {
            report_output_error(context_ref, error);
        }
    }
}

unsafe fn render_output(
    context: *mut OutputContext,
    buffer: *mut objc2_audio_toolbox::AudioQueueBuffer,
) {
    if context.is_null() || buffer.is_null() {
        return;
    }
    let context = unsafe { &*context };
    if context.is_stopping() {
        return;
    }
    let audio_buffer = unsafe { &mut *buffer };
    let sample_count = audio_buffer.mAudioDataBytesCapacity as usize / size_of::<f32>();
    let samples = unsafe {
        slice::from_raw_parts_mut(audio_buffer.mAudioData.as_ptr().cast::<f32>(), sample_count)
    };
    samples.fill(0.0);
    let mut callback_guard = context
        .sample_callback
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let callback_panicked = callback_guard.as_mut().is_some_and(|callback_fn| {
        catch_unwind(AssertUnwindSafe(|| callback_fn(samples))).is_err()
    });
    if callback_panicked {
        *callback_guard = None;
        drop(callback_guard);
        report_output_error(context, AudioError::CallbackPanicked);
    }
    audio_buffer.mAudioDataByteSize = audio_buffer.mAudioDataBytesCapacity;
}

fn report_input_error(context: &InputContext, error: AudioError) {
    if context.is_stopping() {
        return;
    }
    let mut callback_guard = context
        .error_callback
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let callback_panicked = callback_guard
        .as_mut()
        .is_some_and(|callback_fn| catch_unwind(AssertUnwindSafe(|| callback_fn(error))).is_err());
    if callback_panicked {
        *callback_guard = None;
    }
}

fn report_output_error(context: &OutputContext, error: AudioError) {
    if context.is_stopping() {
        return;
    }
    let mut callback_guard = context
        .error_callback
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let callback_panicked = callback_guard
        .as_mut()
        .is_some_and(|callback_fn| catch_unwind(AssertUnwindSafe(|| callback_fn(error))).is_err());
    if callback_panicked {
        *callback_guard = None;
    }
}
