//! Runtime evidence for camera capture on this machine.
//!
//! Requires a physical camera, so it is ignored by default:
//! `cargo test -p nexo-video --test camera_capture -- --ignored --nocapture`
//!
//! Windows: Media Foundation backend. Linux: V4L2 backend.

use nexo_video::{VideoCaptureSource, enumerate_cameras};

#[test]
#[ignore = "requires a physical camera"]
fn opens_camera_and_reads_a_frame() {
    let cameras = enumerate_cameras().expect("enumerar cameras");
    let Some(camera) = cameras.first() else {
        eprintln!("no camera available; skipping");
        return;
    };

    let mut source =
        VideoCaptureSource::open_with_resolution(&camera.id, 640, 480).expect("abrir camera");
    let (width, height) = source.resolution();
    assert!(
        width > 0 && height > 0,
        "resolucao invalida {width}x{height}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let frame = loop {
        if let Some(frame) = source.read_frame().expect("ler frame") {
            break frame;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "camera nao entregou um frame no prazo"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(!frame.data.is_empty(), "frame vazio");
    assert_eq!(frame.width, width);
    assert_eq!(frame.height, height);
    eprintln!(
        "captured {}x{} {:?} bytes={}",
        frame.width,
        frame.height,
        frame.format,
        frame.data.len()
    );
}
