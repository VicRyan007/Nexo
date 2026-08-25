use std::time::Duration;

use nexo_video::HardwareH264Encoder;

fn main() {
    let width = 640_u32;
    let height = 480_u32;
    let mut encoder = match HardwareH264Encoder::new(width, height, 1_500_000) {
        Ok(encoder) => encoder,
        Err(error) => {
            println!("H.264 hardware encoder unavailable: {error}");
            return;
        }
    };
    let frame_size = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|luma| luma.checked_add(luma / 2))
        .expect("smoke-test dimensions fit");
    let input = vec![128_u8; frame_size];
    let mut output_count = 0_u32;
    for index in 0..30_u64 {
        match encoder.encode(Duration::from_millis(index * 33), &input) {
            Ok(Some(frame)) => {
                output_count += 1;
                println!(
                    "encoded frame {index}: {} bytes, keyframe={}",
                    frame.data.len(),
                    frame.is_keyframe
                );
            }
            Ok(None) => {}
            Err(error) => {
                println!("H.264 encode failed on frame {index}: {error}");
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("H.264 smoke complete: {output_count} encoded outputs");
}
