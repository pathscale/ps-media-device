//! Exercise the real default output queue with silence, without microphone capture.
use ps_media_device::{StreamConfig, default_output_device, start_output};
use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn main() -> Result<(), Box<dyn Error>> {
    let callbacks = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));
    let callback_count = callbacks.clone();
    let error_count = errors.clone();
    let stream = start_output(
        &default_output_device()?,
        StreamConfig {
            channels: 2,
            ..StreamConfig::default()
        },
        move |samples| {
            samples.fill(0.0);
            callback_count.fetch_add(1, Ordering::Relaxed);
        },
        move |error| {
            eprintln!("output callback error: {error}");
            error_count.fetch_add(1, Ordering::Relaxed);
        },
    )?;
    // Three callbacks prime the queue before start. Require asynchronous delivery too.
    std::thread::sleep(Duration::from_millis(500));
    stream.stop()?;
    let stopped_count = callbacks.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(100));
    if stopped_count <= 3
        || errors.load(Ordering::Relaxed) != 0
        || callbacks.load(Ordering::Relaxed) != stopped_count
    {
        eprintln!(
            "callbacks={stopped_count} after_stop={} errors={}",
            callbacks.load(Ordering::Relaxed),
            errors.load(Ordering::Relaxed)
        );
        return Err(std::io::Error::other("output delivery or synchronous stop failed").into());
    }
    println!("silent_output_callbacks={stopped_count} stopped=true errors=0");
    Ok(())
}
