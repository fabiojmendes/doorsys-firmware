//! Reusable ISR-driven Wiegand protocol reader for `esp-hal`.
//!
//! Captures falling-edge pulses on `D0` and `D1` inside a GPIO interrupt handler so individual
//! ~50 µs pulses are never missed while the async executor is busy with Wi-Fi or flash operations,
//! then frames and decodes 4-bit keypad presses and 26-bit RFID fobs asynchronously.

use core::{
    cell::RefCell,
    sync::atomic::{AtomicBool, Ordering},
};

use critical_section::Mutex;
use defmt::{info, warn};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::{
    gpio::{Event, Input, InputConfig, InputPin, Io, Pull},
    handler, ram,
};

/// Quiet window after the last falling edge before a Wiegand frame is considered complete.
const WIEGAND_FRAME_TIMEOUT: Duration = Duration::from_millis(50);

/// Polling interval while waiting for the frame quiet window to elapse.
const WIEGAND_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Maximum number of bytes buffered per Wiegand frame (32 bits).
pub const BUFFER_SIZE: usize = 4;

struct IsrState {
    d0: Input<'static>,
    d1: Input<'static>,
    bits: usize,
    data: [u8; BUFFER_SIZE],
    last_edge: Option<Instant>,
}

static ISR_STATE: Mutex<RefCell<Option<IsrState>>> = Mutex::new(RefCell::new(None));
static EDGE_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static READER_TAKEN: AtomicBool = AtomicBool::new(false);

#[handler]
#[ram]
fn gpio_isr() {
    critical_section::with(|cs| {
        let mut borrow = ISR_STATE.borrow_ref_mut(cs);
        let Some(state) = borrow.as_mut() else {
            return;
        };

        let d0_trig = state.d0.is_interrupt_set();
        let d1_trig = state.d1.is_interrupt_set();
        state.d0.clear_interrupt();
        state.d1.clear_interrupt();

        if d0_trig == d1_trig {
            return;
        }

        if state.bits < state.data.len() * 8 {
            let value: u8 = if d0_trig { 0 } else { 0x80 };
            state.data[state.bits / 8] |= value >> (state.bits % 8);
            state.bits += 1;
        }

        state.last_edge = Some(Instant::now());
        EDGE_SIGNAL.signal(());
    });
}

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
    _private: (),
}

impl Reader {
    /// Configure `d0_pin` and `d1_pin` with internal pull-ups and falling-edge interrupts, and
    /// install the GPIO interrupt handler on `io`.
    ///
    /// # Panics
    ///
    /// Panics if called more than once (the GPIO interrupt vector is chip-global).
    pub fn new(
        io: &mut Io<'_>,
        d0_pin: impl InputPin + 'static,
        d1_pin: impl InputPin + 'static,
    ) -> Self {
        critical_section::with(|_cs| {
            assert!(
                !READER_TAKEN.load(Ordering::Relaxed),
                "Wiegand Reader already initialized"
            );
            READER_TAKEN.store(true, Ordering::Relaxed);
        });

        let pull_up = InputConfig::default().with_pull(Pull::Up);
        let mut d0 = Input::new(d0_pin, pull_up);
        let mut d1 = Input::new(d1_pin, pull_up);

        io.set_interrupt_handler(gpio_isr);

        critical_section::with(|cs| {
            d0.listen(Event::FallingEdge);
            d1.listen(Event::FallingEdge);
            *ISR_STATE.borrow_ref_mut(cs) = Some(IsrState {
                d0,
                d1,
                bits: 0,
                data: [0; BUFFER_SIZE],
                last_edge: None,
            });
        });

        Self { _private: () }
    }

    /// Wait for the next complete Wiegand packet.
    ///
    /// This method is cancel-safe: if dropped by `with_timeout` before any edge arrives, no bits
    /// are lost.
    pub async fn read(&mut self) -> Packet {
        loop {
            EDGE_SIGNAL.wait().await;

            loop {
                Timer::after(WIEGAND_POLL_INTERVAL).await;

                let frame = critical_section::with(|cs| {
                    let mut borrow = ISR_STATE.borrow_ref_mut(cs);
                    let state = borrow.as_mut()?;
                    let last = state.last_edge?;
                    if Instant::now().duration_since(last) >= WIEGAND_FRAME_TIMEOUT {
                        let bits = state.bits;
                        let data = state.data;
                        state.bits = 0;
                        state.data = [0; BUFFER_SIZE];
                        state.last_edge = None;
                        EDGE_SIGNAL.reset();
                        Some((bits, data))
                    } else {
                        None
                    }
                });

                if let Some((bits, data)) = frame {
                    if bits > 0 {
                        return Packet::decode(bits, data);
                    }
                    break;
                }
            }
        }
    }
}
