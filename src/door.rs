//! Door relay driver and async task logic.

use defmt::info;
use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Timer};
use esp_hal::gpio::Output;

/// Duration the door relay stays energized after a valid unlock trigger.
pub const DOOR_OPEN_DELAY: Duration = Duration::from_secs(4);

static DOOR_TRIGGER: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Trigger the door relay to open (or extend the open window if already open).
pub fn trigger_open() {
    DOOR_TRIGGER.signal(());
}

/// Container encapsulating the physical door relay output pin.
pub struct Door<'d> {
    driver: Output<'d>,
}

impl<'d> Door<'d> {
    pub fn new(driver: Output<'d>) -> Self {
        Self { driver }
    }

    pub fn open(&mut self) {
        self.driver.set_high();
    }

    pub fn close(&mut self) {
        self.driver.set_low();
    }

    /// Wait for unlock signals, energize the relay for [`DOOR_OPEN_DELAY`], and reset the timer if
    /// re-triggered while open (matching the original firmware's queue-draining behavior).
    pub async fn run(mut self) -> ! {
        loop {
            DOOR_TRIGGER.wait().await;
            info!("Opening door relay");
            self.open();

            while matches!(
                select(DOOR_TRIGGER.wait(), Timer::after(DOOR_OPEN_DELAY)).await,
                Either::First(())
            ) {}

            info!("Closing door relay");
            self.close();
        }
    }
}
