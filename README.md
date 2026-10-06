# ps-media-device

`ps-media-device` is a macOS-only Rust crate for discovering CoreAudio devices and opening
explicit input or output streams. It also exposes read-only AVFoundation camera inventory. The
first stream API uses interleaved `f32` PCM and pins a queue to the selected device UID.

This is an initial backend. It does not provide device-change notifications, loopback capture,
per-device audio format negotiation, or a cross-platform fallback.

## Device probes

Run `cargo run --example coreaudio-probe` on macOS to enumerate CoreAudio and print only the
number of devices, directional counts, and whether each system default exists. It does not print
device names or UIDs and does not open a capture or playback stream.

`cameras()` returns AVFoundation metadata for the built-in wide-angle and external cameras it
currently discovers. Each item includes its persistent ID, display name, device type, and whether
AVFoundation marks it as the default. Enumeration does not create a capture input or session,
start capture, or request camera authorization.

`start_camera_capture(&CameraDeviceId)` opens one exact persistent ID and returns a pull-based
capture handle. It requires macOS camera permission to have already been granted; the function
never requests permission or falls back to another camera. The host still needs an
`NSCameraUsageDescription` and the camera entitlement when sandboxed.

The capture output is BGRA8. The session selects the highest available standard preset at or
below 1920x1080, and each delivered pixel buffer is checked against that limit before it is
copied. Frames own tightly packed pixel data and a monotonic callback-delivery timestamp. The
handle queues at most two frames and drops the oldest queued frame when a newer one arrives.
`next_frame(timeout)` is pull-based; `stop()` stops the session, detaches the delegate, and waits
for the serial callback queue to drain. Dropping the handle performs the same synchronous stop.
Unsupported authorization states, formats, layouts, or oversized delivered frames fail closed.

```rust,no_run
use std::time::Duration;

use ps_media_device::{cameras, start_camera_capture};

fn first_frame() -> Result<(), Box<dyn std::error::Error>> {
    let camera = cameras()?
        .into_iter()
        .next()
        .ok_or_else(|| std::io::Error::other("no camera is available"))?;
    let mut capture = start_camera_capture(camera.id())?;
    let _frame = capture.next_frame(Duration::from_secs(2))?;
    capture.stop();
    Ok(())
}
```

## API

```rust,no_run
use ps_media_device::{StreamConfig, devices, start_input};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = devices()?
        .into_iter()
        .find(|device| device.has_input())
        .ok_or_else(|| std::io::Error::other("no audio input device"))?;

    let _stream = start_input(
        &input,
        StreamConfig::default(),
        |interleaved_f32| {
            // Copy or process this buffer quickly. It is called on CoreAudio's queue thread.
            let _sample_count = interleaved_f32.len();
        },
        |error| eprintln!("capture stopped: {error}"),
    )?;

    // Keep `_stream` alive while capture is needed; dropping it stops the queue.
    Ok(())
}
```

For playback, call `start_output` with an output-capable device and a callback that fills every
sample in each supplied mutable slice. `default_input_device` and `default_output_device` return
the current system defaults. `AudioDevice::uid()` is the persistent identifier; numeric
`AudioDeviceId` values may change when the audio server restarts.

Callbacks execute on AudioQueue-owned threads. Keep them short and non-blocking. Callback
panics are caught: a panicking sample callback is disabled, capture continues to requeue buffers,
and playback emits silence. Stream handles are deliberately `!Send` and `!Sync`: safe Rust cannot
put one in an `Arc<Mutex<Option<_>>>` captured by its own `Send` callback and destroy the queue
from inside that callback. Opening another stream from an audio callback is rejected at runtime.

The host application must provide a nonempty `NSMicrophoneUsageDescription` in its app bundle and
grant microphone permission before capture. `preflight_input_permission()` reports whether both
conditions hold without opening an input or requesting permission. `start_input()` runs this
preflight before creating an AudioQueue input, so it will not trigger a permission prompt.

Camera frame capture is provided by the separate capture API; device enumeration remains
read-only and does not request camera authorization or start capture.

## Implementation and provenance

The backend calls CoreAudio and AudioToolbox through the published `objc2-*` Rust bindings. It
does not use a C/C++ implementation, bindgen build script, sibling checkout dependency, or
vendored native library. Cargo dependencies use caret ranges; this repository intentionally has
no committed lockfile.

The implementation was informed by read-only audits of:

- CPAL `0.19.0`, Apache-2.0, upstream snapshot
  `79275c2313da30e9f8b1ad8168385d091a2c9ada`. Its macOS backend demonstrates CoreAudio device
  property queries and CoreFoundation device UIDs. CPAL's stream layer uses `coreaudio-rs`; that
  path was not imported here.
- Nokhwa `0.11.0`, MIT OR Apache-2.0, upstream snapshot
  `0ceddb5a8526055642a246b2d9be3cc0a564ddf4`. Its macOS bindings target AVFoundation camera
  capture and bring older native sys dependencies, so they are not used for audio.

No upstream source files are vendored in this crate. `LICENSE-MIT` and `LICENSE-APACHE` retain
the PathScale dual-license texts.

## Current verification limits

The macOS enumeration probe compiled and found 10 audio devices, including default input and
output, without printing names or UIDs. The first silent-output probe exposed a callback
re-enqueue race during stop. After adding the stopping fence, the probe delivered 49 callbacks,
reported zero errors, and observed no callbacks after synchronous stop. Run
`cargo run --example silent-output-probe` to repeat this real output-queue check; it emits silence
and does not open a microphone.

Capture permission behavior, input delivery, explicit device selection, route changes, and
disposal-failure handling still require macOS integration evidence. Successful output callbacks
do not prove microphone capture, audible playback, camera frames, or browser integration.

The camera enumeration probe also compiled and found six cameras with a default present.
`cargo run --example camera-probe` reports counts only and never opens a capture session.
Frame capture compiled and passed all-target clippy with warnings denied; hardware permission, frame delivery,
explicit device selection, stop/drain behavior, and output format still need macOS integration
checks.
Formatting also passed for the frame-capture path. Microphone preflight can be inspected without
opening a device with `cargo run --example input-permission-probe`.
