//! Keypad and RFID fob access-control state machine.
//!
//! Reads packets from the Wiegand [`Reader`], validates PINs and fobs against [`UserDb`], triggers
//! the door relay on valid entry, plays audible/visual feedback on the keypad signal line, and
//! enqueues [`Audit`] records for MQTT publication.

use defmt::{info, warn};
use doorsys_protocol::{Audit, CodeType, Timestamp};
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Timer};
use esp_hal::gpio::Output;

use crate::{
    clock, door, mqtt,
    user_db::UserDb,
    wiegand::{Packet, Reader},
};

const MAX_PIN_LENGTH: usize = 8;
const PIN_TIMEOUT: Duration = Duration::from_secs(10);
const STAR_KEY: u8 = 0x0A;
const HASH_KEY: u8 = 0x0B;

/// Convert a sequence of decimal digit key presses into an `i32`.
pub fn keys_to_int(keys: &[u8]) -> i32 {
    keys.iter()
        .copied()
        .rev()
        .enumerate()
        .fold(0i32, |acc, (i, num)| {
            acc + 10i32.pow(i as u32) * i32::from(num)
        })
}

/// Play audible/LED feedback on the keypad's open-drain signal pin (`GPIO7`).
///
/// Solid low for 800 ms on success; 8 toggles at 100 ms intervals on failure, returning high when
/// finished.
async fn keypad_feedback(success: bool, pin: &mut Output<'_>) {
    for _ in 0..8 {
        if success {
            pin.set_low();
        } else {
            pin.toggle();
        }
        Timer::after(Duration::from_millis(100)).await;
    }
    pin.set_high();
}

fn enqueue_audit(code: i32, code_type: CodeType, success: bool) {
    mqtt::enqueue_audit(Audit {
        timestamp: Timestamp {
            secs_since_epoch: clock::now_unix_secs(),
            nanos_since_epoch: 0,
        },
        code,
        code_type,
        success,
    });
}

/// Run the Wiegand reader and keypad PIN entry loop forever.
pub async fn run(mut reader: Reader, mut signal_pin: Output<'static>, user_db: UserDb) -> ! {
    signal_pin.set_high();
    let mut keys: heapless::Vec<u8, MAX_PIN_LENGTH> = heapless::Vec::new();

    loop {
        match select(reader.read(), Timer::after(PIN_TIMEOUT)).await {
            Either::First(Packet::Key { key }) => {
                if key == HASH_KEY {
                    let pin = keys_to_int(&keys);
                    let success = user_db.contains(pin).await;
                    info!("PIN entered {}: valid={}", pin, success);
                    if success {
                        door::trigger_open();
                    }
                    enqueue_audit(pin, CodeType::Pin, success);
                    keys.clear();
                    keypad_feedback(success, &mut signal_pin).await;
                } else if key == STAR_KEY {
                    info!("PIN entry cancelled");
                    keys.clear();
                    keypad_feedback(false, &mut signal_pin).await;
                } else if keys.is_full() {
                    warn!("PIN sequence exceeded max length: {:?}", keys.as_slice());
                    keys.clear();
                    keypad_feedback(false, &mut signal_pin).await;
                } else {
                    let _ = keys.push(key);
                }
            }
            Either::First(Packet::Card { rfid }) => {
                let success = user_db.contains(rfid).await;
                info!("RFID fob tapped {}: valid={}", rfid, success);
                if success {
                    door::trigger_open();
                }
                enqueue_audit(rfid, CodeType::Fob, success);
                keypad_feedback(success, &mut signal_pin).await;
                keys.clear();
            }
            Either::First(Packet::Unknown { bits, data }) => {
                warn!(
                    "Unrecognized Wiegand pattern: bits={}, data={:02X}",
                    bits, data
                );
            }
            Either::Second(()) => {
                if !keys.is_empty() {
                    warn!("Incomplete PIN sequence timed out: {:?}", keys.as_slice());
                    keys.clear();
                    keypad_feedback(false, &mut signal_pin).await;
                }
            }
        }
    }
}
