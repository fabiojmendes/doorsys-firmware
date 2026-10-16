//! Application configuration types (`WifiConfig` and `MqttConfig`) and persistence helpers.

use defmt::warn;
use serde::{Deserialize, Serialize};

use crate::{
    storage::{SharedStorage, StorageError, StorageKey},
    wifi::WifiCredentials,
};

/// Default MQTT TCP port when not specified in the broker URL.
pub const DEFAULT_MQTT_PORT: u16 = 1883;

/// Stored MQTT broker configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, defmt::Format)]
pub struct MqttConfig {
    pub url: heapless::String<96>,
    pub username: heapless::String<64>,
    pub password: heapless::String<64>,
}

impl MqttConfig {
    /// Parse `mqtt://host[:port]` (or bare `host[:port]`) into `(host, port)`.
    pub fn parse_endpoint(&self) -> Option<(heapless::String<64>, u16)> {
        parse_mqtt_url(self.url.as_str())
    }
}

/// Parse an `mqtt://host[:port]` URL into `(host, port)`.
pub fn parse_mqtt_url(url: &str) -> Option<(heapless::String<64>, u16)> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }

    let rest = if let Some(after) = trimmed.strip_prefix("mqtt://") {
        after
    } else if trimmed.contains("://") {
        return None;
    } else {
        trimmed
    };

    let host_port = rest.split('/').next().unwrap_or(rest);
    if host_port.is_empty() {
        return None;
    }

    let (host_str, port) = if let Some((h, p)) = host_port.rsplit_once(':') {
        let parsed_port: u16 = p.parse().ok()?;
        if parsed_port == 0 {
            return None;
        }
        (h, parsed_port)
    } else {
        (host_port, DEFAULT_MQTT_PORT)
    };

    if host_str.is_empty() {
        return None;
    }

    let host = heapless::String::try_from(host_str).ok()?;
    Some((host, port))
}

/// Load Wi-Fi credentials from flash storage.
pub async fn load_wifi(storage: &'static SharedStorage) -> Option<WifiCredentials> {
    match storage.lock().await.get(StorageKey::WifiConfig).await {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!("Failed to read Wi-Fi config from flash: {:?}", e);
            None
        }
    }
}

/// Save Wi-Fi credentials to flash storage.
pub async fn save_wifi(
    storage: &'static SharedStorage,
    creds: &WifiCredentials,
) -> Result<(), StorageError> {
    storage
        .lock()
        .await
        .set(StorageKey::WifiConfig, creds)
        .await
}

/// Load MQTT configuration from flash storage.
pub async fn load_mqtt(storage: &'static SharedStorage) -> Option<MqttConfig> {
    match storage.lock().await.get(StorageKey::MqttConfig).await {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!("Failed to read MQTT config from flash: {:?}", e);
            None
        }
    }
}

/// Save MQTT configuration to flash storage.
pub async fn save_mqtt(
    storage: &'static SharedStorage,
    cfg: &MqttConfig,
) -> Result<(), StorageError> {
    storage.lock().await.set(StorageKey::MqttConfig, cfg).await
}
