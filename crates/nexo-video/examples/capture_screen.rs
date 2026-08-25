//! Capture a few frames from the primary monitor and print their metadata.
//!
//! Windows Graphics Capture needs the console to keep focus while running.
//! Run with: `cargo run -p nexo-video --example capture_screen`

use nexo_video::{ScreenCaptureSource, enumerate_monitors};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let monitors = enumerate_monitors()?;
    println!("Monitores: {}", monitors.len());

    let Some(target) = monitors
        .iter()
        .find(|monitor| monitor.is_primary)
        .or_else(|| monitors.first())
    else {
        println!("Nenhum monitor encontrado.");
        return Ok(());
    };
    println!(
        "Abrindo: {} [{}] ({}x{})",
        target.name,
        if target.is_primary {
            "primario"
        } else {
            "secundario"
        },
        target.width,
        target.height
    );

    let mut source = ScreenCaptureSource::open_monitor(&target.id)?;
    println!("Resolucao de captura: {:?}", source.resolution());

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
            None => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
    println!("Frames capturados: {frames}");
    Ok(())
}
