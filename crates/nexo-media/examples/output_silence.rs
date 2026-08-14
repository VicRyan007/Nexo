use std::{thread, time::Duration};

use nexo_media::{AudioFrame, OPUS_FRAME_SAMPLES, OPUS_SAMPLE_RATE, OutputPlayback};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = OutputPlayback::start_default()?;
    output.play(&AudioFrame {
        samples: vec![0.0; OPUS_FRAME_SAMPLES],
        sample_rate: OPUS_SAMPLE_RATE,
    })?;
    thread::sleep(Duration::from_millis(100));
    println!("default output opened and accepted 48 kHz call audio");
    Ok(())
}
