use nexo_media::enumerate_audio_devices;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for device in enumerate_audio_devices()? {
        println!(
            "{:?}\tdefault={}\t{} Hz\t{} ch\t{}\t{}",
            device.kind,
            device.is_default,
            device.sample_rate,
            device.channels,
            device.sample_format,
            device.name
        );
    }
    Ok(())
}
