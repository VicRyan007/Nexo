//! Runtime evidence for screen capture on this machine.
//!
//! Requires a desktop session with DWM running and the test process in the
//! foreground (Windows Graphics Capture), so it is ignored by default:
//! `cargo test -p nexo-video --test screen_capture -- --ignored --nocapture`

#![cfg(windows)]

use nexo_video::{ScreenCaptureSource, enumerate_monitors};

#[test]
#[ignore = "requires a desktop session and foreground focus"]
fn opens_primary_monitor_and_reads_a_frame() {
    let monitors = enumerate_monitors().expect("enumerar monitores");
    let Some(monitor) = monitors.iter().find(|monitor| monitor.is_primary) else {
        eprintln!("no monitor available; skipping");
        return;
    };

    let mut source = ScreenCaptureSource::open_monitor(&monitor.id).expect("abrir captura de tela");
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
            "captura de tela nao entregou um frame no prazo"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(!frame.data.is_empty(), "frame vazio");
    assert_eq!(frame.width, width);
    assert_eq!(frame.height, height);
    assert_eq!(
        frame.data.len(),
        (width as usize) * (height as usize) * 4,
        "BGRA8 deve ter 4 bytes por pixel"
    );
    eprintln!(
        "captured {}x{} {:?} bytes={}",
        frame.width,
        frame.height,
        frame.format,
        frame.data.len()
    );
}
