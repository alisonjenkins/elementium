//! Print the camera list exactly as the application sees it.
//!
//! `wpctl status` shows what `PipeWire` has; this shows what our enumeration makes of it,
//! which is not the same question and has been the difference between a working call and a
//! black picture. The page picks a `deviceId` out of this list, and until recently that
//! choice never reached Rust at all (the constraint was `snake_case` on one side and
//! `camelCase` on the other), so which entry comes first here now decides which camera a
//! default-configured client opens.
//!
//! Reports node id, name, description and device path. No frames are captured.
//!
//! ```text
//! nix develop -c cargo run -p elementium-media --example list_video_sources
//! ```

fn main() {
    match elementium_media::pipewire_nodes::list_video_sources() {
        Ok(sources) if sources.is_empty() => {
            println!("PipeWire enumerated no video sources at all");
        }
        Ok(sources) => {
            println!("{} video source(s), in the order the page is offered them:", sources.len());
            for (index, s) in sources.iter().enumerate() {
                println!(
                    "  [{index}] node {}  {}  ({})  path={}",
                    s.node_id,
                    s.name,
                    s.description,
                    s.device_path.as_deref().unwrap_or("-"),
                );
            }
        }
        Err(e) => println!("enumeration failed: {e}"),
    }
}
