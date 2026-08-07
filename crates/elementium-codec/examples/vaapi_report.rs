//! Print what this machine can encode in hardware.
//!
//! The probe decides which codecs are offered in SDP, so being able to see its answer
//! without running a call is what makes a wrong answer diagnosable. Compare against
//! `vainfo`: they should agree, and if they do not the probe is wrong.
//!
//! ```bash
//! cargo run -p elementium-codec --example vaapi_report
//! ```

#![allow(clippy::print_stdout)]

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    println!("all encoders this machine reports:");
    for cap in elementium_codec::available_encoders() {
        println!(
            "  {:<5} {:<16} up to {}x{}",
            cap.codec.sdp_name(),
            cap.backend.name(),
            cap.max_width,
            cap.max_height
        );
    }

    println!();
    println!("negotiation order at 1280x720 (best first):");
    for codec in elementium_codec::hardware::negotiation_order(1280, 720, &[]) {
        let backend = elementium_codec::best_backend(codec, 1280, 720);
        println!("  {:<5} via {}", codec.sdp_name(), backend.name());
    }
}
