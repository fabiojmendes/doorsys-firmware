use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use esp_idf_svc::nvs::{EspNvs, EspNvsPartition, NvsDefault};

const NVS_NAMESPACE: &str = "codes";

/// Abstraction to encapsulate the persistent database of users
#[derive(Clone)]
pub struct UserDB(Arc<Mutex<UserData>>);

/// The database of users is BTreeSet of i32s that is persistet to
/// flash memory everytime it is changed. On reset the memory structure
/// will be loaded from flash again.
struct UserData {
    nvs: EspNvs<NvsDefault>,
    codes: BTreeSet<i32>,
}

fn persist(data: &mut UserData) -> anyhow::Result<()> {
    let buf = postcard::to_allocvec(&data.codes).context("encoding failure")?;
    data.nvs
        .set_raw(NVS_NAMESPACE, &buf)
        .context("nvs failure")?;
    Ok(())
}

impl UserDB {
    pub fn new(nvs_part: EspNvsPartition<NvsDefault>) -> anyhow::Result<Self> {
        let nvs = EspNvs::new(nvs_part, "doorsys", true)?;
        let blob_size = nvs.blob_len(NVS_NAMESPACE)?.unwrap_or(0);
        let mut buf = vec![0; blob_size];
        let maybe_blob = nvs
            .get_raw(NVS_NAMESPACE, &mut buf)
            .context("error loading nvs")?;

        match maybe_blob {
            Some(slice) => {
                let codes = postcard::from_bytes(slice).context("error deconding blob")?;
                let data = UserData { nvs, codes };

                log::info!(
                    "Loaded {} codes from flash ({} bytes)",
                    data.codes.len(),
                    slice.len()
                );
                Ok(UserDB(Arc::new(Mutex::new(data))))
            }
            None => {
                log::warn!("No codes found, starting blank");
                Ok(UserDB(Arc::new(Mutex::new(UserData {
                    nvs,
                    codes: BTreeSet::new(),
                }))))
            }
        }
    }

    pub fn add(&self, code: i32) -> anyhow::Result<()> {
        let mut data = self.0.lock().unwrap();
        data.codes.insert(code);
        persist(&mut data)?;
        Ok(())
    }

    pub fn bulk(&self, codes: Vec<i32>) -> anyhow::Result<()> {
        let mut data = self.0.lock().unwrap();
        data.codes = BTreeSet::from_iter(codes);
        persist(&mut data)?;
        Ok(())
    }

    pub fn replace(&self, old: i32, new: i32) -> anyhow::Result<()> {
        let mut data = self.0.lock().unwrap();
        data.codes.remove(&old);
        data.codes.insert(new);
        persist(&mut data)?;
        Ok(())
    }

    pub fn contains(&self, code: i32) -> bool {
        let data = self.0.lock().unwrap();
        data.codes.contains(&code)
    }

    pub fn delete(&self, code: i32) -> anyhow::Result<()> {
        let mut data = self.0.lock().unwrap();
        data.codes.remove(&code);
        persist(&mut data)?;
        Ok(())
    }
}
