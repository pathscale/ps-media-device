use std::error::Error;

use ps_media_device::{default_input_device, default_output_device, devices};

fn main() -> Result<(), Box<dyn Error>> {
    let devices = devices()?;
    let input_count = devices.iter().filter(|device| device.has_input()).count();
    let output_count = devices.iter().filter(|device| device.has_output()).count();

    println!(
        "coreaudio_devices={} input_devices={} output_devices={} default_input_present={} default_output_present={}",
        devices.len(),
        input_count,
        output_count,
        default_input_device().is_ok(),
        default_output_device().is_ok(),
    );

    Ok(())
}
