//! Periodic health check reporter publishing heap and flash storage metrics to `doorsys/status`
//! in InfluxDB line protocol format.

use core::fmt::Write as _;
use defmt::{info, warn};
use embassy_time::{Duration, Timer};

use crate::{
    clock,
    mqtt::{self, MAX_TELEMETRY_LEN},
    storage::SharedStorage,
};

const HEALTH_INTERVAL: Duration = Duration::from_secs(60);

/// Log and enqueue a formatted line, or warn if it did not fit in the buffer.
fn publish_line(line: &str, fmt_ok: bool) {
    if !fmt_ok {
        warn!("Health line exceeded {} bytes; dropping", MAX_TELEMETRY_LEN);
        return;
    }
    info!("{}", line);
    mqtt::publish_telemetry(line);
}

/// Run the periodic health check loop forever.
pub async fn run(
    net_id: heapless::String<32>,
    version: &'static str,
    storage: &'static SharedStorage,
) -> ! {
    let capacity = storage.lock().await.capacity();
    let mut line = heapless::String::<MAX_TELEMETRY_LEN>::new();

    loop {
        let time = clock::now_nanos();

        let stats = esp_alloc::HEAP.stats();
        let free = stats.size.saturating_sub(stats.current_usage);
        let used = stats.current_usage;
        let total = stats.size;
        let max_used = stats.max_usage;

        line.clear();
        let ok = write!(
            line,
            "heap,host={net_id},version={version} free={free},used={used},total={total},max_used={max_used} {time}"
        )
        .is_ok();
        publish_line(&line, ok);

        line.clear();
        let ok = write!(
            line,
            "storage,host={net_id},version={version} capacity={capacity} {time}"
        )
        .is_ok();
        publish_line(&line, ok);

        Timer::after(HEALTH_INTERVAL).await;
    }
}
