//! Periodic health check reporter publishing heap and flash storage metrics to `doorsys/status`
//! in InfluxDB line protocol format.

use alloc::{format, vec::Vec};
use defmt::info;
use embassy_time::{Duration, Timer};

use crate::{
    clock,
    mqtt::{self, PublishQoS},
    storage::SharedStorage,
};

const HEALTH_INTERVAL: Duration = Duration::from_secs(60);

/// Run the periodic health check loop forever.
pub async fn run(
    net_id: heapless::String<32>,
    version: &'static str,
    storage: &'static SharedStorage,
) -> ! {
    let capacity = storage.lock().await.capacity();

    loop {
        let time = clock::now_nanos();

        let stats = esp_alloc::HEAP.stats();
        let free = stats.size.saturating_sub(stats.current_usage);
        let used = stats.current_usage;
        let total = stats.size;
        let max_used = stats.max_usage;

        let heap_line = format!(
            "heap,host={net_id},version={version} free={free},used={used},total={total},max_used={max_used} {time}"
        );
        info!("{}", heap_line.as_str());
        mqtt::try_enqueue(
            "doorsys/status",
            Vec::from(heap_line.as_bytes()),
            PublishQoS::AtMostOnce,
        );

        let storage_line =
            format!("storage,host={net_id},version={version} capacity={capacity} {time}");
        info!("{}", storage_line.as_str());
        mqtt::try_enqueue(
            "doorsys/status",
            Vec::from(storage_line.as_bytes()),
            PublishQoS::AtMostOnce,
        );

        Timer::after(HEALTH_INTERVAL).await;
    }
}
