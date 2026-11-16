//! Pull-based, bounded AVFoundation camera frame capture.

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::slice;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, ClassType, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureDevice, AVCaptureDeviceInput, AVCaptureOutput,
    AVCaptureSession, AVCaptureSessionPreset640x480, AVCaptureSessionPreset1280x720,
    AVCaptureSessionPreset1920x1080, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetDataSize, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA,
    kCVReturnSuccess,
};
use objc2_foundation::{NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString};

use crate::macos_camera::CameraDeviceId;

const MAX_FRAME_WIDTH: usize = 1920;
const MAX_FRAME_HEIGHT: usize = 1080;
const FRAME_QUEUE_CAPACITY: usize = 2;

/// A capture startup or frame-delivery failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CameraCaptureError {
    /// Camera access is not already authorized. Capture never prompts for permission.
    PermissionNotAuthorized,
    /// The selected persistent camera ID is no longer available.
    DeviceUnavailable,
    /// AVFoundation could not open the selected camera as a capture input.
    DeviceInputUnavailable,
    /// The host bundle does not provide a nonempty NSCameraUsageDescription.
    MissingCameraUsageDescription,
    /// The selected camera does not support a session preset within the frame-size cap.
    UnsupportedSessionPreset,
    /// AVFoundation cannot add the camera input or video output to a session.
    UnsupportedSessionConfiguration,
    /// The camera output does not advertise BGRA8 pixel buffers.
    UnsupportedPixelFormat,
    /// AVFoundation did not start the capture session.
    SessionStartFailed,
    /// A delivered frame exceeded the configured maximum dimensions.
    FrameExceedsMaximumDimensions,
    /// A delivered sample did not contain a CoreVideo image buffer.
    ImageBufferUnavailable,
    /// A delivered pixel buffer had an unsupported pixel format.
    DeliveredPixelFormatMismatch,
    /// CoreVideo refused to lock a delivered pixel buffer.
    PixelBufferLockFailed,
    /// The pixel buffer exposed an inconsistent or oversized byte layout.
    InvalidPixelBufferLayout,
    /// The pixel buffer could not be unlocked after copying.
    PixelBufferUnlockFailed,
    /// A bounded frame allocation failed.
    FrameAllocationFailed,
    /// A panic occurred while copying a capture callback frame.
    FrameCallbackFailed,
}

impl std::fmt::Display for CameraCaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::PermissionNotAuthorized => "camera permission has not already been granted",
            Self::DeviceUnavailable => "the selected camera is unavailable",
            Self::DeviceInputUnavailable => "AVFoundation could not open the selected camera",
            Self::MissingCameraUsageDescription => {
                "the host bundle must provide NSCameraUsageDescription before camera capture"
            }
            Self::UnsupportedSessionPreset => {
                "the selected camera has no supported preset within 1920x1080"
            }
            Self::UnsupportedSessionConfiguration => {
                "AVFoundation rejected the camera session configuration"
            }
            Self::UnsupportedPixelFormat => "the selected camera does not support BGRA8 output",
            Self::SessionStartFailed => "AVFoundation did not start the capture session",
            Self::FrameExceedsMaximumDimensions => "a camera frame exceeded 1920x1080",
            Self::ImageBufferUnavailable => "the camera sample did not contain an image buffer",
            Self::DeliveredPixelFormatMismatch => "the camera delivered a non-BGRA8 pixel buffer",
            Self::PixelBufferLockFailed => "CoreVideo could not lock the camera pixel buffer",
            Self::InvalidPixelBufferLayout => "CoreVideo returned an invalid pixel buffer layout",
            Self::PixelBufferUnlockFailed => "CoreVideo could not unlock the camera pixel buffer",
            Self::FrameAllocationFailed => "could not allocate a bounded camera frame",
            Self::FrameCallbackFailed => "the camera frame callback failed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CameraCaptureError {}

/// An owned, tightly packed BGRA8 camera frame.
#[derive(Debug)]
pub struct CameraFrame {
    width: u32,
    height: u32,
    timestamp: Duration,
    bgra: Vec<u8>,
}

impl CameraFrame {
    /// Frame width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Frame height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Tightly packed row length in bytes.
    pub fn bytes_per_row(&self) -> usize {
        self.width as usize * 4
    }

    /// Monotonic elapsed time from capture startup to callback delivery.
    pub fn timestamp(&self) -> Duration {
        self.timestamp
    }

    /// Owned BGRA8 pixel data in top-to-bottom row order.
    pub fn bgra(&self) -> &[u8] {
        &self.bgra
    }
}

struct FrameQueue {
    frames: VecDeque<CameraFrame>,
    closed: bool,
    failure: Option<CameraCaptureError>,
    last_timestamp: Duration,
}

struct CaptureShared {
    started_at: Instant,
    queue: Mutex<FrameQueue>,
    ready: Condvar,
}

impl CaptureShared {
    fn new() -> Self {
        Self {
            started_at: Instant::now(),
            queue: Mutex::new(FrameQueue {
                frames: VecDeque::with_capacity(FRAME_QUEUE_CAPACITY),
                closed: false,
                failure: None,
                last_timestamp: Duration::ZERO,
            }),
            ready: Condvar::new(),
        }
    }

    fn receive_sample(&self, sample: &CMSampleBuffer) -> Result<(), CameraCaptureError> {
        if lock(&self.queue).closed {
            return Ok(());
        }
        let image =
            unsafe { sample.image_buffer() }.ok_or(CameraCaptureError::ImageBufferUnavailable)?;
        let buffer: &CVPixelBuffer = &image;
        let width = CVPixelBufferGetWidth(buffer);
        let height = CVPixelBufferGetHeight(buffer);
        if width == 0 || height == 0 || width > MAX_FRAME_WIDTH || height > MAX_FRAME_HEIGHT {
            return Err(CameraCaptureError::FrameExceedsMaximumDimensions);
        }
        if CVPixelBufferGetPixelFormatType(buffer) != kCVPixelFormatType_32BGRA {
            return Err(CameraCaptureError::DeliveredPixelFormatMismatch);
        }

        let pixel_lock = PixelBufferReadLock::acquire(buffer)?;
        let copy_result = copy_bgra_frame(buffer, width, height);
        let unlock_result = pixel_lock.release();
        let bgra = match (copy_result, unlock_result) {
            (_, Err(error)) => return Err(error),
            (Err(error), Ok(())) => return Err(error),
            (Ok(bytes), Ok(())) => bytes,
        };

        let mut queue = lock(&self.queue);
        if queue.closed {
            return Ok(());
        }
        let mut timestamp = self.started_at.elapsed();
        if timestamp <= queue.last_timestamp {
            timestamp = queue.last_timestamp.saturating_add(Duration::from_nanos(1));
        }
        queue.last_timestamp = timestamp;
        if queue.frames.len() == FRAME_QUEUE_CAPACITY {
            queue.frames.pop_front();
        }
        queue.frames.push_back(CameraFrame {
            width: width as u32,
            height: height as u32,
            timestamp,
            bgra,
        });
        self.ready.notify_one();
        Ok(())
    }

    fn fail(&self, error: CameraCaptureError) {
        let mut queue = lock(&self.queue);
        if queue.failure.is_none() {
            queue.frames.clear();
            queue.failure = Some(error);
            queue.closed = true;
            self.ready.notify_all();
        }
    }

    fn close(&self) {
        let mut queue = lock(&self.queue);
        queue.closed = true;
        self.ready.notify_all();
    }

    fn next_frame(&self, timeout: Duration) -> Result<Option<CameraFrame>, CameraCaptureError> {
        let started = Instant::now();
        let mut queue = lock(&self.queue);
        loop {
            if let Some(error) = queue.failure {
                return Err(error);
            }
            if let Some(frame) = queue.frames.pop_front() {
                return Ok(Some(frame));
            }
            if queue.closed {
                return Ok(None);
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Ok(None);
            }
            match self.ready.wait_timeout(queue, remaining) {
                Ok((next, wait)) => {
                    queue = next;
                    if wait.timed_out() && queue.frames.is_empty() && queue.failure.is_none() {
                        return Ok(None);
                    }
                }
                Err(poisoned) => queue = poisoned.into_inner().0,
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct PixelBufferReadLock<'a> {
    buffer: &'a CVPixelBuffer,
    locked: bool,
}

impl<'a> PixelBufferReadLock<'a> {
    fn acquire(buffer: &'a CVPixelBuffer) -> Result<Self, CameraCaptureError> {
        // SAFETY: `buffer` is a live retained sample image for this callback and remains borrowed
        // until the matching unlock. CoreVideo requires locking before accessing its base address.
        let result =
            unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) };
        if result != kCVReturnSuccess {
            return Err(CameraCaptureError::PixelBufferLockFailed);
        }
        Ok(Self {
            buffer,
            locked: true,
        })
    }

    fn release(mut self) -> Result<(), CameraCaptureError> {
        // SAFETY: `acquire` successfully locked this still-live buffer with the same flags, and
        // this guard has not yet unlocked it.
        let result = unsafe {
            CVPixelBufferUnlockBaseAddress(self.buffer, CVPixelBufferLockFlags::ReadOnly)
        };
        if result == kCVReturnSuccess {
            self.locked = false;
            Ok(())
        } else {
            Err(CameraCaptureError::PixelBufferUnlockFailed)
        }
    }
}

impl Drop for PixelBufferReadLock<'_> {
    fn drop(&mut self) {
        if self.locked {
            // SAFETY: This is the fallback paired unlock for a successful `acquire`; the borrowed
            // buffer remains live and the same read-only flags are used.
            let _ = unsafe {
                CVPixelBufferUnlockBaseAddress(self.buffer, CVPixelBufferLockFlags::ReadOnly)
            };
        }
    }
}

fn copy_bgra_frame(
    buffer: &CVPixelBuffer,
    width: usize,
    height: usize,
) -> Result<Vec<u8>, CameraCaptureError> {
    let packed_row_bytes = width
        .checked_mul(4)
        .ok_or(CameraCaptureError::InvalidPixelBufferLayout)?;
    let output_len = packed_row_bytes
        .checked_mul(height)
        .ok_or(CameraCaptureError::InvalidPixelBufferLayout)?;
    let source_stride = CVPixelBufferGetBytesPerRow(buffer);
    let source_len = source_stride
        .checked_mul(height)
        .ok_or(CameraCaptureError::InvalidPixelBufferLayout)?;
    let data_size = CVPixelBufferGetDataSize(buffer);
    let base = CVPixelBufferGetBaseAddress(buffer).cast::<u8>();
    if source_stride < packed_row_bytes || source_len > data_size || base.is_null() {
        return Err(CameraCaptureError::InvalidPixelBufferLayout);
    }

    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(output_len)
        .map_err(|_| CameraCaptureError::FrameAllocationFailed)?;
    bytes.resize(output_len, 0);
    for row in 0..height {
        let source_offset = row
            .checked_mul(source_stride)
            .ok_or(CameraCaptureError::InvalidPixelBufferLayout)?;
        let target_offset = row
            .checked_mul(packed_row_bytes)
            .ok_or(CameraCaptureError::InvalidPixelBufferLayout)?;
        let source = unsafe { slice::from_raw_parts(base.add(source_offset), packed_row_bytes) };
        bytes[target_offset..target_offset + packed_row_bytes].copy_from_slice(source);
    }
    Ok(bytes)
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AnyThread]
    #[ivars = Arc<CaptureShared>]
    struct CameraSampleDelegate;

    unsafe impl NSObjectProtocol for CameraSampleDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CameraSampleDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        unsafe fn capture_output_did_output_sample_buffer_from_connection(
            &self,
            _output: &AVCaptureOutput,
            sample: &CMSampleBuffer,
            _connection: &objc2_av_foundation::AVCaptureConnection,
        ) {
            let result = catch_unwind(AssertUnwindSafe(|| self.ivars().receive_sample(sample)));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => self.ivars().fail(error),
                Err(_) => self.ivars().fail(CameraCaptureError::FrameCallbackFailed),
            }
        }
    }
);

impl CameraSampleDelegate {
    fn new(shared: Arc<CaptureShared>) -> Retained<Self> {
        let allocated = Self::alloc().set_ivars(shared);
        // SAFETY: NSObject's init is the designated initializer for this subclass.
        unsafe { msg_send![super(allocated), init] }
    }
}

/// A running capture pinned to one persistent AVFoundation camera ID.
///
/// Captured frames are copied to owned BGRA8 buffers. At most two frames are queued; a new
/// frame drops the oldest queued one. The handle is intentionally `!Send` and `!Sync` so
/// `stop` and destruction stay on the creating thread and can synchronously drain callbacks.
pub struct CameraCapture {
    session: Option<Retained<AVCaptureSession>>,
    input: Option<Retained<AVCaptureDeviceInput>>,
    output: Option<Retained<AVCaptureVideoDataOutput>>,
    delegate: Option<Retained<CameraSampleDelegate>>,
    callback_queue: Option<DispatchRetained<DispatchQueue>>,
    shared: Arc<CaptureShared>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl CameraCapture {
    /// Wait for one owned frame. Returns `Ok(None)` on timeout or after stop and queue drain.
    pub fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<CameraFrame>, CameraCaptureError> {
        let result = self.shared.next_frame(timeout);
        if result.is_err() {
            self.stop();
        }
        result
    }

    /// Stop capture and synchronously drain the serial delegate queue.
    pub fn stop(&mut self) {
        if let Some(session) = self.session.as_ref() {
            if unsafe { session.isRunning() } {
                unsafe { session.stopRunning() };
            }
        }
        if let (Some(output), Some(callback_queue)) =
            (self.output.as_ref(), self.callback_queue.as_ref())
        {
            unsafe { output.setSampleBufferDelegate_queue(None, None) };
            callback_queue.exec_sync(|| {});
        }
        self.shared.close();
        if let Some(session) = self.session.as_ref() {
            if let Some(output) = self.output.as_ref() {
                unsafe { session.removeOutput(output.as_super()) };
            }
            if let Some(input) = self.input.as_ref() {
                unsafe { session.removeInput(input.as_super()) };
            }
        }
        self.delegate.take();
        self.output.take();
        self.input.take();
        self.session.take();
        self.callback_queue.take();
    }
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start capture from the exact camera ID, requiring camera permission to have already been
/// granted by the host. This function never requests permission or chooses a different camera.
/// AVFoundation startup is synchronous and can block; call this on the device owner thread,
/// not the UI thread, and keep the returned handle on that same thread.
pub fn start_camera_capture(
    device_id: &CameraDeviceId,
) -> Result<CameraCapture, CameraCaptureError> {
    let media_type = unsafe { AVMediaTypeVideo }.ok_or(CameraCaptureError::DeviceUnavailable)?;
    let authorization = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };
    if authorization != AVAuthorizationStatus::Authorized {
        return Err(CameraCaptureError::PermissionNotAuthorized);
    }

    let usage_description_key = NSString::from_str("NSCameraUsageDescription");
    let has_usage_description = objc2_foundation::NSBundle::mainBundle()
        .objectForInfoDictionaryKey(&usage_description_key)
        .and_then(|value| value.downcast::<NSString>().ok())
        .is_some_and(|value| !value.to_string().trim().is_empty());
    if !has_usage_description {
        return Err(CameraCaptureError::MissingCameraUsageDescription);
    }

    let requested_id = NSString::from_str(device_id.as_str());
    let device = unsafe { AVCaptureDevice::deviceWithUniqueID(&requested_id) }
        .ok_or(CameraCaptureError::DeviceUnavailable)?;
    if unsafe { device.uniqueID() }.to_string() != device_id.as_str()
        || !unsafe { device.isConnected() }
    {
        return Err(CameraCaptureError::DeviceUnavailable);
    }

    let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }
        .map_err(|_| CameraCaptureError::DeviceInputUnavailable)?;
    let session = unsafe { AVCaptureSession::new() };
    if !unsafe { session.canAddInput(input.as_super()) } {
        return Err(CameraCaptureError::UnsupportedSessionConfiguration);
    }
    unsafe { session.addInput(input.as_super()) };

    let output = unsafe { AVCaptureVideoDataOutput::new() };
    if !unsafe { session.canAddOutput(output.as_super()) } {
        return Err(CameraCaptureError::UnsupportedSessionConfiguration);
    }
    unsafe { session.addOutput(output.as_super()) };

    let preset = [
        unsafe { AVCaptureSessionPreset1920x1080 },
        unsafe { AVCaptureSessionPreset1280x720 },
        unsafe { AVCaptureSessionPreset640x480 },
    ]
    .into_iter()
    .find(|preset| unsafe { session.canSetSessionPreset(preset) })
    .ok_or(CameraCaptureError::UnsupportedSessionPreset)?;
    unsafe { session.setSessionPreset(preset) };

    let formats = unsafe { output.availableVideoCVPixelFormatTypes() }.to_vec();
    if !formats
        .iter()
        .any(|format| format.unsignedIntValue() == kCVPixelFormatType_32BGRA)
    {
        return Err(CameraCaptureError::UnsupportedPixelFormat);
    }

    let pixel_format_key = unsafe {
        // SAFETY: CoreVideo documents this CFString as toll-free bridged with NSString; the
        // AVFoundation videoSettings dictionary uses NSString keys.
        &*(std::ptr::from_ref(kCVPixelBufferPixelFormatTypeKey) as *const NSString)
    };
    let pixel_format_number = NSNumber::numberWithUnsignedInt(kCVPixelFormatType_32BGRA);
    // SAFETY: NSNumber is an Objective-C object and AnyObject accepts any Objective-C object.
    let pixel_format_value = unsafe { Retained::cast_unchecked::<AnyObject>(pixel_format_number) };
    let video_settings = NSDictionary::from_slices(&[pixel_format_key], &[&*pixel_format_value]);
    unsafe { output.setVideoSettings(Some(&video_settings)) };
    unsafe { output.setAlwaysDiscardsLateVideoFrames(true) };

    let shared = Arc::new(CaptureShared::new());
    let delegate = CameraSampleDelegate::new(shared.clone());
    let callback_queue = DispatchQueue::new(
        "org.pathscale.ps-media-device.camera-frames",
        DispatchQueueAttr::SERIAL,
    );
    unsafe {
        output.setSampleBufferDelegate_queue(
            Some(ProtocolObject::from_ref(&*delegate)),
            Some(&callback_queue),
        );
    }

    let mut capture = CameraCapture {
        session: Some(session),
        input: Some(input),
        output: Some(output),
        delegate: Some(delegate),
        callback_queue: Some(callback_queue),
        shared,
        _not_send_or_sync: PhantomData,
    };
    if let Some(session) = capture.session.as_ref() {
        unsafe { session.startRunning() };
        if !unsafe { session.isRunning() } {
            capture.stop();
            return Err(CameraCaptureError::SessionStartFailed);
        }
    }
    Ok(capture)
}
