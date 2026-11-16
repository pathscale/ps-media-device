use ps_media_device::preflight_input_permission;

fn main() {
    match preflight_input_permission() {
        Ok(()) => println!("input_permission_preflight=ready capture_opened=false"),
        Err(error) => {
            println!("input_permission_preflight=refused reason={error} capture_opened=false")
        }
    }
}
