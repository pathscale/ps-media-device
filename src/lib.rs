//! macOS CoreAudio device discovery and interleaved `f32` PCM streams.
//!
//! The first backend targets macOS only. Device IDs are paired with CoreAudio UIDs so
//! streams can be pinned to a specific device instead of silently following a default.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(target_os = "macos"))]
compile_error!("ps-media-device currently supports macOS only");

mod macos;

pub use macos::{
    AudioDevice, AudioDeviceId, AudioError, AudioInput, AudioOutput, StreamConfig,
    default_input_device, default_output_device, devices, start_input, start_output,
};
