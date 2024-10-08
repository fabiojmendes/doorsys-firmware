// Reference: https://docs.espressif.com/projects/esp-idf/en/latest/esp32/api-reference/system/freertos.html

mod config;
mod door;
mod mqtt;
mod network;
mod user;
mod wiegand;

use config::DoorsysConfig;
use doorsys_protocol::{Audit, CodeType};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{InputPin, Output, OutputPin, PinDriver};
use esp_idf_svc::hal::prelude::Peripherals;
use esp_idf_svc::mqtt::client::QoS;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::{
    esp, gpio_install_isr_service, heap_caps_get_free_size, heap_caps_get_largest_free_block,
    heap_caps_get_minimum_free_size, heap_caps_get_total_size, nvs_get_stats, ESP_INTR_FLAG_IRAM,
    MALLOC_CAP_DEFAULT,
};
use esp_idf_svc::systime::EspSystemTime;
use mqtt::MqttClient;
use std::mem;
use std::ptr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use std::{thread, time::Duration};
use wiegand::Packet;

use crate::user::UserDB;
use crate::wiegand::Reader;

const MAX_PIN_LENGTH: usize = 8;
const PIN_TIMEOUT: Duration = Duration::from_secs(10);
const STAR_KEY: u8 = 0x0A;
const HASH_KEY: u8 = 0x0B;
const DOOR_OPEN_DELAY: Duration = Duration::from_secs(4);

fn setup_door(pin: impl OutputPin, door_rx: Receiver<()>) -> anyhow::Result<()> {
    let mut door = door::Door::new(pin)?;

    thread::spawn(move || loop {
        door_rx.recv().unwrap();
        if let Err(e) = door.open() {
            log::error!("error: {}", e);
        }
        // Drain the queue while the door is open
        while door_rx.recv_timeout(DOOR_OPEN_DELAY).is_ok() {}
        if let Err(e) = door.close() {
            log::error!("error: {}", e);
        }
    });

    Ok(())
}

/// Plays a sound when the door opens or the code is invalid
fn keypad_feedback(
    success: bool,
    pin: &mut PinDriver<'_, impl OutputPin, Output>,
) -> anyhow::Result<()> {
    for _ in 0..8 {
        if success {
            pin.set_low()?;
        } else {
            pin.toggle()?;
        }
        thread::sleep(Duration::from_millis(100));
    }
    pin.set_high()?;
    Ok(())
}

/// Converts a key press sequence into an integer
fn keys_to_int(keys: &[u8]) -> i32 {
    keys.iter()
        .cloned()
        .rev()
        .enumerate()
        .fold(0, |acc, (i, num)| acc + 10i32.pow(i as u32) * num as i32)
}

/// Setup the wiegand reader and spawns a thread to read incoming packets
fn setup_reader(
    door_tx: Sender<()>,
    user_db: UserDB,
    audit_tx: Sender<Audit>,
    d0_gpio: impl InputPin,
    d1_gpio: impl InputPin,
    signal_pin: impl OutputPin,
) -> anyhow::Result<()> {
    let mut signal_driver = PinDriver::output_od(signal_pin)?;
    signal_driver.set_high()?;

    thread::spawn(move || {
        let (_reader, channel) =
            Reader::new(d0_gpio, d1_gpio).expect("Error initializing wiegand reader");

        let mut keys = Vec::with_capacity(MAX_PIN_LENGTH);

        // Reads the queue in a loop.
        // If a pin sequence is not entered in PIN_TIMEOUT time
        // it will be cancelled
        loop {
            match channel.recv_timeout(PIN_TIMEOUT) {
                Ok(Packet::Key { key }) => {
                    if key == HASH_KEY {
                        let pin = keys_to_int(&keys);
                        let success = user_db.contains(pin);
                        log::info!("Valid pin {}: {}", pin, success);
                        if success {
                            door_tx.send(()).unwrap();
                        }
                        let audit = Audit {
                            code: pin,
                            code_type: CodeType::Pin,
                            timestamp: SystemTime::now(),
                            success,
                        };
                        if let Err(e) = audit_tx.send(audit) {
                            log::error!("error sending audit record: {}", e);
                        }
                        keys.clear();
                        if let Err(e) = keypad_feedback(success, &mut signal_driver) {
                            log::warn!("error playing feedback: {}", e);
                        }
                    } else if key == STAR_KEY {
                        log::info!("Cancel sequence");
                        keys.clear();
                        if let Err(e) = keypad_feedback(false, &mut signal_driver) {
                            log::warn!("error playing feedback: {}", e);
                        }
                    } else if keys.len() == MAX_PIN_LENGTH {
                        log::warn!("pin sequence is too big {:?}", keys);
                        keys.clear();
                        if let Err(e) = keypad_feedback(false, &mut signal_driver) {
                            log::warn!("error playing feedback: {}", e);
                        }
                    } else {
                        keys.push(key);
                    }
                }
                Ok(Packet::Card { rfid }) => {
                    let success = user_db.contains(rfid);
                    log::info!("Valid rfid {}: {}", rfid, success);
                    if success {
                        door_tx.send(()).unwrap();
                    }
                    let audit = Audit {
                        code: rfid,
                        code_type: CodeType::Fob,
                        timestamp: SystemTime::now(),
                        success,
                    };
                    if let Err(e) = audit_tx.send(audit) {
                        log::error!("error sending audit record: {}", e);
                    }
                    if let Err(e) = keypad_feedback(success, &mut signal_driver) {
                        log::warn!("error playing feedback: {}", e);
                    }
                    keys.clear();
                }
                Ok(Packet::Unknown { bits, data }) => {
                    log::warn!("pattern not recognized bits: {}, data: {:02X?}", bits, data);
                }
                Err(_e) => {
                    if !keys.is_empty() {
                        log::warn!("incomplete pin sequence {:?}", keys);
                        keys.clear();
                        if let Err(e) = keypad_feedback(false, &mut signal_driver) {
                            log::warn!("error playing feedback: {}", e);
                        }
                    }
                }
            }
        }
    });

    Ok(())
}

/// Publishes mqtt audit events
fn setup_audit_publiher(
    device_id: &str,
    mqtt_client: Arc<Mutex<MqttClient>>,
    audit_rx: Receiver<Audit>,
) {
    let topic = format!("doorsys/audit/{device_id}");
    thread::spawn(move || {
        for audit in audit_rx {
            match postcard::to_allocvec(&audit) {
                Ok(buffer) => {
                    if let Err(e) = mqtt_client.lock().unwrap().enqueue(
                        &topic,
                        QoS::AtLeastOnce,
                        false,
                        &buffer,
                    ) {
                        log::error!("error sending audit: {}", e);
                    }
                }
                Err(e) => {
                    log::error!("error encoding audit: {}", e);
                }
            }
        }
    });
}

/// Starts the health check thread
fn health_check(net_id: &str, mqtt_client: Arc<Mutex<MqttClient>>) -> anyhow::Result<()> {
    let systime = EspSystemTime {};

    let mqtt_client = mqtt_client.clone();

    let net_id = net_id.to_owned();
    let version = built_info::GIT_VERSION.unwrap_or("");

    thread::spawn(move || loop {
        let time = systime.now().as_nanos();
        let heap = unsafe {
            let total = heap_caps_get_total_size(MALLOC_CAP_DEFAULT);
            let free = heap_caps_get_free_size(MALLOC_CAP_DEFAULT);
            let minimum = heap_caps_get_minimum_free_size(MALLOC_CAP_DEFAULT);
            let largest_free = heap_caps_get_largest_free_block(MALLOC_CAP_DEFAULT);
            format!("heap,host={net_id},version={version} free={free},total={total},minimum={minimum},largest_free={largest_free} {time}")
        };
        log::info!("{}", heap);
        if let Err(e) = mqtt_client.lock().unwrap().publish(
            "doorsys/status",
            QoS::AtMostOnce,
            false,
            heap.as_bytes(),
        ) {
            log::warn!("mqtt publish error: {}", e);
        }

        let nvs = unsafe {
            let mut stats = mem::MaybeUninit::uninit();
            if let Err(e) = esp!(nvs_get_stats(ptr::null(), stats.as_mut_ptr())) {
                format!("error: {}", e)
            } else {
                let stats = stats.assume_init();
                let used = stats.used_entries;
                let free = stats.free_entries;
                let total = stats.total_entries;
                format!("nvs,host={net_id},version={version} used={used},free={free},total={total} {time}")
            }
        };
        log::info!("{}", nvs);
        if let Err(e) = mqtt_client.lock().unwrap().publish(
            "doorsys/status",
            QoS::AtMostOnce,
            false,
            nvs.as_bytes(),
        ) {
            log::warn!("mqtt publish error: {}", e);
        }

        thread::sleep(Duration::from_secs(60));
    });

    Ok(())
}

fn main() -> anyhow::Result<()> {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();
    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!(
        "Doorsys version {}, built for {} by {}.",
        built_info::PKG_VERSION,
        built_info::TARGET,
        built_info::RUSTC_VERSION
    );
    if let (Some(version), Some(hash), Some(dirty)) = (
        built_info::GIT_VERSION,
        built_info::GIT_COMMIT_HASH_SHORT,
        built_info::GIT_DIRTY,
    ) {
        log::info!("Git version: {version} ({hash}) dirty: {dirty}");
    }

    // Installs the generic GPIO interrupt handler which will
    // be used later on by the wiegand reader.
    esp!(unsafe { gpio_install_isr_service(ESP_INTR_FLAG_IRAM as i32) })?;

    let peripherals = Peripherals::take().unwrap();
    let sysloop = EspSystemEventLoop::take()?;
    let nvs_part = EspDefaultNvsPartition::take()?;

    let mut doorsys_config = DoorsysConfig::new(nvs_part.clone())?;

    let user_db = UserDB::new(nvs_part.clone())?;

    log::info!("Starting application");

    let (door_tx, door_rx) = mpsc::channel();
    setup_door(peripherals.pins.gpio10, door_rx)?;

    let (audit_tx, audit_rx) = mpsc::channel();
    setup_reader(
        door_tx.clone(),
        user_db.clone(),
        audit_tx,
        peripherals.pins.gpio4,
        peripherals.pins.gpio5,
        peripherals.pins.gpio7,
    )?;

    let net_id = network::setup_wireless(
        peripherals.modem,
        sysloop.clone(),
        nvs_part.clone(),
        &mut doorsys_config,
    )?;

    let mqtt_client = mqtt::setup_mqtt(
        &net_id,
        user_db.clone(),
        &doorsys_config.read_mqtt_configs()?,
    )?;

    setup_audit_publiher(&net_id, mqtt_client.clone(), audit_rx);

    health_check(&net_id, mqtt_client.clone())?;

    log::info!("Application fully functional");

    Ok(())
}

pub mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}
