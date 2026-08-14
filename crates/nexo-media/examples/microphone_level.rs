use std::time::Duration;

use nexo_media::InputLevelMonitor;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let monitor = InputLevelMonitor::start_default()?;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        println!("{:.6}", monitor.level());
    }
    Ok(())
}
