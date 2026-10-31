//! Async Wiegand protocol reader for `esp-hal`.
//!
//! Captures falling-edge pulses on `D0` and `D1` using async GPIO edge waits with a short glitch
//! filter delay, then frames and decodes 4-bit keypad presses and 26-bit RFID fobs.

use defmt::{info, warn};
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Timer};
use esp_hal::{
    gpio::{Input, InputConfig, InputPin, Pull},
    peripherals::IO_MUX,
};

/// Quiet window after the last falling edge before a Wiegand frame is considered complete.
const WIEGAND_FRAME_TIMEOUT: Duration = Duration::from_millis(50);

/// Maximum number of bytes buffered per Wiegand frame (32 bits).
const BUFFER_SIZE: usize = 4;

/// Check parity bits 25 (even) and 0 (odd) on a 26-bit Wiegand frame.
///
/// Reference: <https://getsafeandsound.com/blog/26-bit-wiegand-format/>
pub fn parity_check_26bits(mut rfid: u32) -> bool {
    // Odd parity is checked over the rightmost 13 bits.
    let mut count = 0;
    for _ in 0..13 {
        count += rfid & 1;
        rfid >>= 1;
    }
    if count.is_multiple_of(2) {
        return false;
    }

    // Even parity is checked over the leftmost 13 bits.
    let mut count = 0;
    for _ in 0..13 {
        count += rfid & 1;
        rfid >>= 1;
    }
    count.is_multiple_of(2)
}

/// Decoded packet read from the Wiegand interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, defmt::Format)]
pub enum Packet {
    /// 4-bit keypad key press.
    Key { key: u8 },
    /// 26-bit Wiegand RFID card/fob code.
    Card { rfid: i32 },
    /// Unrecognized bit pattern or parity failure.
    Unknown {
        bits: usize,
        data: [u8; BUFFER_SIZE],
    },
}

impl Packet {
    /// Decode a raw bit count and MSB-packed buffer into a [`Packet`].
    pub fn decode(bits: usize, data: [u8; BUFFER_SIZE]) -> Self {
        info!("Wiegand data received; bits: {}, data: {:02X}", bits, data);
        match bits {
            4 => Self::Key { key: data[0] >> 4 },
            26 => {
                let mut rfid: u32 = (u32::from(data[0]) << 24)
                    | (u32::from(data[1]) << 16)
                    | (u32::from(data[2]) << 8)
                    | u32::from(data[3]);

                // Remove padding bits.
                rfid >>= 6;

                if !parity_check_26bits(rfid) {
                    warn!("Wiegand 26-bit parity check failed");
                    return Self::Unknown { bits, data };
                }

                // Remove parity check bits.
                rfid &= !(1 << 25);
                rfid >>= 1;

                Self::Card { rfid: rfid as i32 }
            }
            _ => Self::Unknown { bits, data },
        }
    }
}

/// Async Wiegand reader bound to two GPIO input pins (`D0` and `D1`).
pub struct Reader {
    d0: Input<'static>,
    d1: Input<'static>,
}

impl Reader {
    /// Configure `d0_pin` and `d1_pin` with internal pull-ups and hardware pad glitch filters.
    pub fn new(d0_pin: impl InputPin + 'static, d1_pin: impl InputPin + 'static) -> Self {
        info!(
            "Creating reader with pins {} and {}",
            d0_pin.number(),
            d1_pin.number()
        );
        [d0_pin.number(), d1_pin.number()]
            .into_iter()
            .for_each(|gpio| {
                IO_MUX::regs()
                    .gpio(gpio as usize)
                    .modify(|_, w| w.filter_en().set_bit());
            });

        let pull_up = InputConfig::default().with_pull(Pull::Up);
        let d0 = Input::new(d0_pin, pull_up);
        let d1 = Input::new(d1_pin, pull_up);

        Self { d0, d1 }
    }

    /// Wait for the next complete Wiegand packet.
    pub async fn read(&mut self) -> Packet {
        let mut bits: usize = 0;
        let mut data = [0u8; BUFFER_SIZE];

        loop {
            let edge = select(
                self.d0.wait_for_falling_edge(),
                self.d1.wait_for_falling_edge(),
            );

            if bits == 0 {
                edge.await;
            } else if let Either::Second(()) =
                select(edge, Timer::after(WIEGAND_FRAME_TIMEOUT)).await
            {
                return Packet::decode(bits, data);
            }

            let d0_trig = self.d0.is_low();
            let d1_trig = self.d1.is_low();

            if d0_trig == d1_trig {
                continue;
            }

            if bits < data.len() * 8 {
                let value: u8 = if d0_trig { 0 } else { 0x80 };
                data[bits / 8] |= value >> (bits % 8);
                bits += 1;
            }
        }
    }
}
