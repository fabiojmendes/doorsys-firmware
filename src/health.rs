//! Periodic health check collector publishing heap, storage, and general status metrics via MQTT.

use defmt::info;
use embassy_time::{Duration, Timer};

use crate::{clock, mqtt, storage::SharedStorage, user_db::UserDb};

const HEALTH_INTERVAL: Duration = Duration::from_secs(60);

/// Snapshot of device health and status metrics collected periodically for telemetry publication.
#[derive(Debug, Clone, Copy, defmt::Format)]
pub struct Telemetry {
    pub version: &'static str,
    pub timestamp_nanos: u128,
    pub uptime_secs: u64,
    pub codes_count: usize,
    pub heap_free: usize,
    pub heap_used: usize,
    pub heap_total: usize,
    pub heap_max_used: usize,
    pub storage_capacity: u32,
}

/// Run the periodic health check loop forever.
pub async fn run(version: &'static str, storage: &'static SharedStorage, user_db: UserDb) -> ! {
    let storage_capacity = storage.lock().await.capacity();

    loop {
        let timestamp_nanos = clock::now_nanos();
        let uptime_secs = clock::uptime_secs();
        let codes_count = user_db.count().await.unwrap_or(0);

        let stats = esp_alloc::HEAP.stats();
        let heap_free = stats.size.saturating_sub(stats.current_usage);
        let heap_used = stats.current_usage;
        let heap_total = stats.size;
        let heap_max_used = stats.max_usage;

        let snapshot = Telemetry {
            version,
            timestamp_nanos,
            uptime_secs,
            codes_count,
            heap_free,
            heap_used,
            heap_total,
            heap_max_used,
            storage_capacity,
        };

        info!("Health telemetry: {:?}", snapshot);
        mqtt::publish_telemetry(snapshot);

        Timer::after(HEALTH_INTERVAL).await;
    }
}
