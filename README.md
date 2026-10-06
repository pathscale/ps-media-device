# ps-media-device

`ps-media-device` is a macOS-only Rust crate for discovering CoreAudio devices and opening
explicit input or output streams. The first stream API uses interleaved `f32` PCM and pins a
queue to the selected device UID.

This is an initial backend. It does not provide camera capture, device-change notifications,
loopback capture, per-device format negotiation, or a cross-platform fallback.

## Device probe

Run `cargo run --example coreaudio-probe` on macOS to enumerate CoreAudio and print only the
number of devices, directional counts, and whether each system default exists. It does not print
device names or UIDs and does not open a capture or playback stream.

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

The host application must provide `NSMicrophoneUsageDescription` in its app bundle. This crate
does not request or manage app permissions itself. The first capture attempt can trigger macOS's
microphone authorization flow.

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

Source was formatted, but no build or device capture was run as part of this preparation. The
CoreAudio FFI signatures, target-specific dependency features, capture permission prompt, device
selection, input delivery, output delivery, and stream teardown still need macOS integration
verification. In particular, the AudioQueue callback lifecycle must be checked on a real macOS
host before treating this backend as release-ready.
