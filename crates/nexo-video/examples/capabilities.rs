//! Print the video capabilities and camera list of this machine.
//!
//! Run with: `cargo run -p nexo-video --example capabilities`

use nexo_video::{CapabilityProbe, enumerate_cameras};

fn main() {
    match enumerate_cameras() {
        Ok(cameras) => {
            println!("Cameras: {}", cameras.len());
            for camera in &cameras {
                println!("  {} ({})", camera.name, camera.id);
            }
        }
        Err(error) => println!("Camera enumeration: {error}"),
    }

    let report = CapabilityProbe::new().probe();
    println!(
        "GPU: {}",
        report.gpu_name.as_deref().unwrap_or("(desconhecida)")
    );
    println!("Runtime pronta: {}", report.runtime_ready);
    println!("Capture backends: {:?}", report.capture);
    for codec in &report.codecs {
        println!(
            "Codec: {} | {:?} | encode={} decode={}",
            codec.name, codec.acceleration, codec.encode, codec.decode
        );
    }
    if let Some(best) = report.preferred_video_encoder() {
        println!("Encoder preferido: {} ({})", best.name, best.acceleration);
    }
}
