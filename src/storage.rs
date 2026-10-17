//! Reusable flash-backed key-value storage built on `esp-storage` and `sequential-storage`.
//!
//! Looks up the `Data/Nvs` partition in the ESP-IDF partition table at runtime and exposes a
//! power-fail-safe typed key-value API.

use alloc::vec::Vec;
use defmt::{info, warn};
use embassy_embedded_hal::adapter::BlockingAsync;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use esp_bootloader_esp_idf::partitions::{
    DataPartitionSubType, PARTITION_TABLE_MAX_LEN, PartitionType, read_partition_table,
};
use esp_hal::peripherals::FLASH;
use esp_storage::{FlashStorage, FlashStorageError};
use sequential_storage::{
    Error as SeqError,
    cache::{
        Cache, Uncached as UncachedKeys, page_pointers::ArrayPagePointers,
        page_states::ArrayPageStates,
    },
    map::{Key, MapConfig, MapStorage, PostcardValue, SerializationError},
};
use serde::{Serialize, de::DeserializeOwned};

/// Maximum number of 4 KiB flash pages tracked by the page cache (32 pages = up to 128 KiB partition).
const MAX_CACHED_PAGES: usize = 32;

/// Internal scratch buffer size for serializing/deserializing a single map item.
const DATA_BUF_SIZE: usize = 512;

const TAG_WIFI_CONFIG: u8 = 0x01;
const TAG_MQTT_CONFIG: u8 = 0x02;
const TAG_USER_CODE: u8 = 0x10;

/// Typed key space used inside the flash map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, defmt::Format)]
pub enum StorageKey {
    /// Wi-Fi station credentials (`1` byte key on flash).
    WifiConfig,
    /// MQTT broker credentials (`1` byte key on flash).
    MqttConfig,
    /// Single user access code (`5` bytes on flash: tag + little-endian `i32`).
    UserCode(i32),
}

impl Key for StorageKey {
    fn serialize_into(&self, buffer: &mut [u8]) -> Result<usize, SerializationError> {
        match *self {
            StorageKey::WifiConfig => {
                let slot = buffer
                    .first_mut()
                    .ok_or(SerializationError::BufferTooSmall)?;
                *slot = TAG_WIFI_CONFIG;
                Ok(1)
            }
            StorageKey::MqttConfig => {
                let slot = buffer
                    .first_mut()
                    .ok_or(SerializationError::BufferTooSmall)?;
                *slot = TAG_MQTT_CONFIG;
                Ok(1)
            }
            StorageKey::UserCode(code) => {
                let out = buffer
                    .get_mut(..5)
                    .ok_or(SerializationError::BufferTooSmall)?;
                out[0] = TAG_USER_CODE;
                out[1..5].copy_from_slice(&code.to_le_bytes());
                Ok(5)
            }
        }
    }

    fn deserialize_from(buffer: &[u8]) -> Result<(Self, usize), SerializationError> {
        let tag = *buffer.first().ok_or(SerializationError::BufferTooSmall)?;
        match tag {
            TAG_WIFI_CONFIG => Ok((StorageKey::WifiConfig, 1)),
            TAG_MQTT_CONFIG => Ok((StorageKey::MqttConfig, 1)),
            TAG_USER_CODE => {
                let bytes: [u8; 4] = buffer
                    .get(1..5)
                    .ok_or(SerializationError::BufferTooSmall)?
                    .try_into()
                    .map_err(|_| SerializationError::InvalidFormat)?;
                Ok((StorageKey::UserCode(i32::from_le_bytes(bytes)), 5))
            }
            _ => Err(SerializationError::InvalidFormat),
        }
    }

    fn get_len(buffer: &[u8]) -> Result<usize, SerializationError> {
        let tag = *buffer.first().ok_or(SerializationError::BufferTooSmall)?;
        match tag {
            TAG_WIFI_CONFIG | TAG_MQTT_CONFIG => Ok(1),
            TAG_USER_CODE => Ok(5),
            _ => Err(SerializationError::InvalidFormat),
        }
    }
}

/// Wrapper implementing [`PostcardValue`] for any serde type.
#[derive(Serialize, serde::Deserialize)]
struct PostcardWrapper<T>(T);

impl<'de, T: Serialize + serde::Deserialize<'de>> PostcardValue<'de> for PostcardWrapper<T> {}

/// Errors produced by [`Storage`].
#[derive(Debug, defmt::Format)]
pub enum StorageError {
    /// Failed to read or locate the `nvs` partition in the partition table.
    PartitionNotFound,
    /// Partition boundaries are invalid for `sequential-storage`.
    InvalidPartitionLayout,
    /// Underlying NOR flash error.
    Flash(FlashStorageError),
    /// Flash partition is full.
    Full,
    /// Flash data was corrupted beyond automatic repair.
    Corrupted,
    /// Item buffer was too small or postcard serialization failed.
    Serialization,
}

impl From<SeqError<FlashStorageError>> for StorageError {
    fn from(err: SeqError<FlashStorageError>) -> Self {
        match err {
            SeqError::Storage { value } => StorageError::Flash(value),
            SeqError::FullStorage => StorageError::Full,
            SeqError::Corrupted {} => StorageError::Corrupted,
            SeqError::BufferTooBig
            | SeqError::BufferTooSmall(_)
            | SeqError::SerializationError(_)
            | SeqError::ItemTooBig => StorageError::Serialization,
            _ => StorageError::Corrupted,
        }
    }
}

type StorageCache = Cache<
    ArrayPageStates<MAX_CACHED_PAGES>,
    ArrayPagePointers<MAX_CACHED_PAGES>,
    UncachedKeys,
    StorageKey,
>;

#[repr(align(4))]
struct AlignedDataBuf([u8; DATA_BUF_SIZE]);

/// Flash-backed key-value store bound to the `nvs` partition.
pub struct Storage {
    map: MapStorage<StorageKey, BlockingAsync<FlashStorage<'static>>, StorageCache>,
    buf: AlignedDataBuf,
    capacity: u32,
}

/// Shared async mutex handle for [`Storage`].
pub type SharedStorage = Mutex<CriticalSectionRawMutex, Storage>;

impl Storage {
    /// Locate the `Data/Nvs` partition on flash, initialize the map, and format the partition if
    /// it still contains legacy ESP-IDF NVS data.
    pub async fn new(flash: FLASH<'static>) -> Result<Self, StorageError> {
        let mut raw_flash = FlashStorage::new(flash);

        let (offset, len) = {
            let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
            let pt = read_partition_table(&mut raw_flash, &mut pt_buf)
                .map_err(|_| StorageError::PartitionNotFound)?;
            let entry = pt
                .find_partition(PartitionType::Data(DataPartitionSubType::Nvs))
                .map_err(|_| StorageError::PartitionNotFound)?
                .ok_or(StorageError::PartitionNotFound)?;
            (entry.offset(), entry.len())
        };

        let page_size = FlashStorage::SECTOR_SIZE;
        let max_len = (MAX_CACHED_PAGES as u32) * page_size;
        let usable_len = len.min(max_len);

        info!(
            "Storage partition at 0x{:x}..0x{:x} ({} bytes)",
            offset,
            offset + usable_len,
            usable_len
        );

        let config =
            MapConfig::<BlockingAsync<FlashStorage<'static>>>::try_new(offset..offset + usable_len)
                .map_err(|_| StorageError::InvalidPartitionLayout)?;

        let cache: StorageCache = Cache::new(
            ArrayPageStates::new(),
            ArrayPagePointers::new(),
            UncachedKeys,
        );

        let mut storage = Self {
            map: MapStorage::new(BlockingAsync::new(raw_flash), config, cache),
            buf: AlignedDataBuf([0u8; DATA_BUF_SIZE]),
            capacity: usable_len,
        };

        // Probe the partition; if it holds legacy ESP-IDF NVS bytes or unrecoverable corruption,
        // format it cleanly so the device can boot and be provisioned.
        match storage
            .map
            .fetch_item::<&[u8]>(&mut storage.buf.0, &StorageKey::WifiConfig)
            .await
        {
            Ok(_) => {}
            Err(SeqError::Corrupted {}) | Err(SeqError::SerializationError(_)) => {
                warn!("Foreign or corrupted data in storage partition; erasing partition");
                storage.map.erase_all().await?;
            }
            Err(e) => return Err(e.into()),
        }

        Ok(storage)
    }

    /// Usable capacity of the underlying flash partition in bytes.
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Fetch and deserialize a value for `key`.
    pub async fn get<T: Serialize + DeserializeOwned>(
        &mut self,
        key: StorageKey,
    ) -> Result<Option<T>, StorageError> {
        let res = self
            .map
            .fetch_item::<PostcardWrapper<T>>(&mut self.buf.0, &key)
            .await?;
        Ok(res.map(|PostcardWrapper(v)| v))
    }

    /// Serialize and store `value` under `key`.
    pub async fn set<T: Serialize + DeserializeOwned>(
        &mut self,
        key: StorageKey,
        value: &T,
    ) -> Result<(), StorageError> {
        let mut raw = [0u8; DATA_BUF_SIZE / 2];
        let encoded: &[u8] =
            postcard::to_slice(value, &mut raw).map_err(|_| StorageError::Serialization)?;
        self.map.store_item(&mut self.buf.0, &key, &encoded).await?;
        Ok(())
    }

    /// Check whether `key` exists in flash without allocating.
    pub async fn contains(&mut self, key: StorageKey) -> Result<bool, StorageError> {
        let found = self
            .map
            .fetch_item::<&[u8]>(&mut self.buf.0, &key)
            .await?
            .is_some();
        Ok(found)
    }

    /// Store a key with an empty payload (used for set-membership items like user codes).
    pub async fn insert_empty(&mut self, key: StorageKey) -> Result<(), StorageError> {
        let empty: &[u8] = &[];
        self.map.store_item(&mut self.buf.0, &key, &empty).await?;
        Ok(())
    }

    /// Remove a single `key` from flash.
    pub async fn remove(&mut self, key: StorageKey) -> Result<(), StorageError> {
        self.map.remove_item(&mut self.buf.0, &key).await?;
        Ok(())
    }

    /// Collect all user codes currently stored in flash, sorted and deduplicated.
    pub async fn list_user_codes(&mut self) -> Result<Vec<i32>, StorageError> {
        let mut codes = Vec::new();
        let mut iter = self.map.fetch_all_items(&mut self.buf.0).await?;
        while let Some((key, _val)) = iter.next::<&[u8]>(&mut self.buf.0).await? {
            if let StorageKey::UserCode(code) = key {
                codes.push(code);
            }
        }
        codes.sort_unstable();
        codes.dedup();
        Ok(codes)
    }

    /// Fetch up to `N` user codes strictly greater than `after` in ascending order without
    /// allocating on the heap.
    pub async fn list_user_codes_page<const N: usize>(
        &mut self,
        after: Option<i32>,
    ) -> Result<heapless::Vec<i32, N>, StorageError> {
        let mut page = heapless::Vec::<i32, N>::new();
        if N == 0 {
            return Ok(page);
        }

        let mut iter = self.map.fetch_all_items(&mut self.buf.0).await?;
        while let Some((key, _val)) = iter.next::<&[u8]>(&mut self.buf.0).await? {
            let StorageKey::UserCode(code) = key else {
                continue;
            };
            if after.is_some_and(|prev| code <= prev) {
                continue;
            }
            if page.is_full() && page.last().is_some_and(|&max| code >= max) {
                continue;
            }
            if let Err(pos) = page.binary_search(&code) {
                if page.is_full() {
                    page.pop();
                }
                let _ = page.insert(pos, code);
            }
        }
        Ok(page)
    }

    /// Count all user codes currently stored in flash without allocating.
    pub async fn count_user_codes(&mut self) -> Result<usize, StorageError> {
        let mut count = 0;
        let mut iter = self.map.fetch_all_items(&mut self.buf.0).await?;
        while let Some((key, _val)) = iter.next::<&[u8]>(&mut self.buf.0).await? {
            if matches!(key, StorageKey::UserCode(_)) {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Erase the entire storage partition.
    pub async fn erase_all(&mut self) -> Result<(), StorageError> {
        self.map.erase_all().await?;
        Ok(())
    }
}
