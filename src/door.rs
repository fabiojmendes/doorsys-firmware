use esp_idf_svc::hal::gpio::{Output, OutputPin, PinDriver};

/// Simple container to encapsulate the door logic
pub struct Door<'d> {
    driver: PinDriver<'d, Output>,
}

impl<'d> Door<'d> {
    pub fn new(pin: impl OutputPin + 'd) -> anyhow::Result<Self> {
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
