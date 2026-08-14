//! Capture a few frames from the first camera and print their metadata.
//!
//! Run with: `cargo run -p nexo-video --example capture_preview`

use nexo_video::{VideoCaptureSource, enumerate_cameras};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cameras = enumerate_cameras()?;
    println!("Cameras: {}", cameras.len());

    let Some(camera) = cameras.first() else {
        println!("Nenhuma camera encontrada.");
        return Ok(());
    };
    println!("Abrindo: {} ({})", camera.name, camera.id);

    let mut source = VideoCaptureSource::open_with_resolution(&camera.id, 640, 480)?;
    println!("Resolucao negociada: {:?}", source.resolution());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut frames = 0;
    while std::time::Instant::now() < deadline && frames < 30 {
        match source.read_frame()? {
            Some(frame) => {
                frames += 1;
                println!(
                    "frame {frames}: {}x{} {:?} ts={:?} bytes={}",
                    frame.width,
                    frame.height,
                    frame.format,
                    frame.timestamp,
                    frame.data.len()
                );
            }
            None => break,
        }
    }
    println!("Frames capturados: {frames}");
    Ok(())
}
