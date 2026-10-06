//! Read-only AVFoundation camera discovery.
//!
//! This module converts discovered Objective-C objects into owned Rust metadata so no
//! `AVCaptureDevice` reference or Objective-C lifetime escapes the call. Enumeration never
//! creates a capture session/input, starts capture, or requests camera authorization. A host
//! still needs to handle camera permission and provide the required usage description before
//! calling the separate camera-capture API. Enumeration itself never prompts for permission.

use std::{error::Error, fmt};

use objc2_av_foundation::{
    AVCaptureDevice, AVCaptureDeviceDiscoverySession, AVCaptureDevicePosition, AVCaptureDeviceType,
    AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureDeviceTypeExternal, AVMediaTypeVideo,
};
use objc2_foundation::NSArray;

/// A persistent AVFoundation identifier for one camera on this Mac.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CameraDeviceId(String);

impl CameraDeviceId {
    /// Return the AVFoundation unique ID. It may identify a connected or virtual camera.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Metadata for a camera visible to AVFoundation at the time of discovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraDevice {
    id: CameraDeviceId,
    name: String,
    device_type: String,
    is_default: bool,
}

impl CameraDevice {
    /// Return the camera's persistent AVFoundation identifier.
    pub fn id(&self) -> &CameraDeviceId {
        &self.id
    }

    /// Return the localized display name reported by AVFoundation.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the AVFoundation device-type string.
    pub fn device_type(&self) -> &str {
        &self.device_type
    }

    /// Whether AVFoundation currently reports this device as the default video device.
    pub fn is_default(&self) -> bool {
        self.is_default
    }
}

/// AVFoundation could not provide the video media type used for camera discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraError;

impl fmt::Display for CameraError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AVFoundation did not provide its video media-type constant")
    }
}

impl Error for CameraError {}

/// Return the current built-in wide-angle and external cameras known to AVFoundation.
///
/// Results are metadata snapshots; devices can connect or disconnect immediately afterward.
/// This function does not create `AVCaptureSession` or `AVCaptureDeviceInput`, call an
/// authorization API, or begin capture. macOS exposes Continuity Cameras as wide-angle
/// cameras by default. A host that opts in to the separate Continuity Camera device type in
/// its Info.plist will need that type added to discovery as well.
pub fn cameras() -> Result<Vec<CameraDevice>, CameraError> {
    let video_media_type = unsafe { AVMediaTypeVideo }.ok_or(CameraError)?;
    let device_types: [&AVCaptureDeviceType; 2] = [
        // These are framework constants; their values are static NSString objects.
        unsafe { AVCaptureDeviceTypeBuiltInWideAngleCamera },
        unsafe { AVCaptureDeviceTypeExternal },
    ];
    let device_types = NSArray::from_slice(&device_types);
    let discovery = unsafe {
        AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
            &device_types,
            Some(video_media_type),
            AVCaptureDevicePosition::Unspecified,
        )
    };

    let default_id = unsafe { AVCaptureDevice::defaultDeviceWithMediaType(video_media_type) }
        .map(|device| unsafe { device.uniqueID() }.to_string());

    let discovered_devices = unsafe { discovery.devices() }.to_vec();
    Ok(discovered_devices
        .into_iter()
        .map(|device| {
            let unique_id = unsafe { device.uniqueID() }.to_string();
            CameraDevice {
                is_default: default_id.as_deref() == Some(unique_id.as_str()),
                id: CameraDeviceId(unique_id),
                name: unsafe { device.localizedName() }.to_string(),
                device_type: unsafe { device.deviceType() }.to_string(),
            }
        })
        .collect())
}

/// Return the current default camera ID, if it is included in [`cameras`].
pub fn default_camera() -> Result<Option<CameraDeviceId>, CameraError> {
    Ok(cameras()?
        .into_iter()
        .find(|device| device.is_default)
        .map(|device| device.id))
}
