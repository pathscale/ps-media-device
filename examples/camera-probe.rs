use ps_media_device::{cameras, default_camera};
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    println!(
        "camera_devices={} default_camera_present={}",
        cameras()?.len(),
        default_camera()?.is_some()
    );
    Ok(())
}
