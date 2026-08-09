//! Who currently holds a camera device open.
//!
//! ```text
//! cargo run -p elementium-media --example camera_holders
//! ```
//!
//! Exists because "Device or resource busy" is the least useful true statement a camera can
//! make, and answering it by hand means `fuser`, then `/proc`, then knowing that half the
//! desktop is called `electron`. This is the same scan the capture path reports with, so
//! running it is also how you check that scan still works on a machine.

fn main() {
    let holders = elementium_media::device_holders::holders_of("/dev/video");
    if holders.is_empty() {
        println!("no process this user owns is holding a camera device");
        return;
    }
    for holder in holders {
        println!("{} <- {}", holder.device, holder.describe());
    }
}
