//! Persistent database of valid access codes stored directly on flash.
//!
//! Each user code is stored as its own `StorageKey::UserCode(code)` entry in `sequential-storage`
//! so the code set can span across all flash pages in the partition without holding a RAM copy.

use alloc::vec::Vec;
use defmt::{error, info};
use doorsys_protocol::UserAction;

use crate::storage::{SharedStorage, StorageError, StorageKey};

/// Flash-backed user code database handle.
#[derive(Clone, Copy)]
pub struct UserDb {
    storage: &'static SharedStorage,
}

impl UserDb {
    pub const fn new(storage: &'static SharedStorage) -> Self {
        Self { storage }
    }

    /// Check whether `code` is present in flash.
    pub async fn contains(&self, code: i32) -> bool {
        let mut store = self.storage.lock().await;
        match store.contains(StorageKey::UserCode(code)).await {
            Ok(found) => found,
            Err(e) => {
                error!("Error checking code {} in flash: {:?}", code, e);
                false
            }
        }
    }

    /// Add `code` to flash if it is not already stored.
    async fn add(&self, code: i32) -> Result<(), StorageError> {
        let mut store = self.storage.lock().await;
        let key = StorageKey::UserCode(code);
        if !store.contains(key).await? {
            store.insert_empty(key).await?;
        }
        Ok(())
    }

    /// Remove `code` from flash.
    async fn delete(&self, code: i32) -> Result<(), StorageError> {
        let mut store = self.storage.lock().await;
        store.remove(StorageKey::UserCode(code)).await
    }

    /// Replace `old` code with `new` code in flash.
    async fn replace(&self, old: i32, new: i32) -> Result<(), StorageError> {
        let mut store = self.storage.lock().await;
        store.remove(StorageKey::UserCode(old)).await?;
        let new_key = StorageKey::UserCode(new);
        if !store.contains(new_key).await? {
            store.insert_empty(new_key).await?;
        }
        Ok(())
    }

    /// Synchronize the flash code database with `codes`, removing stale codes and inserting new
    /// ones without rewriting unchanged entries.
    async fn bulk(&self, mut codes: Vec<i32>) -> Result<(), StorageError> {
        codes.sort_unstable();
        codes.dedup();

        let mut store = self.storage.lock().await;
        let existing = store.list_user_codes().await?;

        for &old_code in &existing {
            if codes.binary_search(&old_code).is_err() {
                store.remove(StorageKey::UserCode(old_code)).await?;
            }
        }

        for &new_code in &codes {
            if existing.binary_search(&new_code).is_err() {
                store.insert_empty(StorageKey::UserCode(new_code)).await?;
            }
        }

        Ok(())
    }

    /// Decode and apply an incoming `doorsys/user` MQTT payload.
    pub async fn process_user_message(&self, data: &[u8]) {
        match postcard::from_bytes::<UserAction>(data) {
            Ok(UserAction::Add(code)) => {
                info!("Adding code {}", code);
                if let Err(e) = self.add(code).await {
                    error!("Error adding code {}: {:?}", code, e);
                }
            }
            Ok(UserAction::Del(code)) => {
                info!("Deleting code {}", code);
                if let Err(e) = self.delete(code).await {
                    error!("Error deleting code {}: {:?}", code, e);
                }
            }
            Ok(UserAction::Replace { old, new }) => {
                info!("Replacing code {} with {}", old, new);
                if let Err(e) = self.replace(old, new).await {
                    error!("Error replacing code {} -> {}: {:?}", old, new, e);
                }
            }
            Ok(UserAction::Bulk(codes)) => {
                info!("Bulk syncing {} codes", codes.len());
                if let Err(e) = self.bulk(codes).await {
                    error!("Error bulk inserting codes: {:?}", e);
                }
            }
            Err(e) => {
                error!("Error decoding UserAction payload: {:?}", e);
            }
        }
    }
}
