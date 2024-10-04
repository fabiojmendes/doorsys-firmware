use esp_idf_svc::hal::gpio::{Output, OutputPin, PinDriver};

/// Simple container to encapsulate the door logic
pub struct Door<'d, T: OutputPin> {
    driver: PinDriver<'d, T, Output>,
}

impl<T: OutputPin> Door<'_, T> {
    pub fn new(pin: T) -> anyhow::Result<Self> {
        let driver = PinDriver::output(pin)?;
        Ok(Door { driver })
    }

    pub fn open(&mut self) -> anyhow::Result<()> {
        Ok(self.driver.set_high()?)
    }

    pub fn close(&mut self) -> anyhow::Result<()> {
        Ok(self.driver.set_low()?)
    }
}
