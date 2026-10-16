//! Reusable async USB-Serial-JTAG interactive shell built on `embedded-cli`.
//!
//! Bridges `embedded-cli`'s synchronous `embedded-io 0.6` writer interface to `esp-hal`'s async
//! `UsbSerialJtag` peripheral so the shell never blocks the Embassy executor while waiting for host
//! terminal reads or writes.

use alloc::vec::Vec;
use core::{cell::RefCell, convert::Infallible};

use embedded_io_async::{Read as AsyncRead, Write as AsyncWrite};
use esp_hal::{
    Async,
    peripherals::USB_DEVICE,
    usb::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx},
};

/// In-memory byte sink implementing `embedded_io_06::Write` for `embedded-cli`.
pub struct BufferedWriter<'a> {
    buf: &'a RefCell<Vec<u8>>,
}

impl<'a> BufferedWriter<'a> {
    pub fn new(buf: &'a RefCell<Vec<u8>>) -> Self {
        Self { buf }
    }
}

impl embedded_io_06::ErrorType for BufferedWriter<'_> {
    type Error = Infallible;
}

impl embedded_io_06::Write for BufferedWriter<'_> {
    fn write(&mut self, data: &[u8]) -> Result<usize, Self::Error> {
        let mut out = self.buf.borrow_mut();
        // Translate bare `\n` to `\r\n` for standard serial terminals when not already preceded by `\r`.
        for &b in data {
            if b == b'\n' && out.last().copied() != Some(b'\r') {
                out.push(b'\r');
            }
            out.push(b);
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Split async USB-Serial-JTAG transport.
pub struct UsbShellIo {
    pub rx: UsbSerialJtagRx<'static, Async>,
    pub tx: UsbSerialJtagTx<'static, Async>,
}

impl UsbShellIo {
    /// Initialize the ESP32-C3 `USB_DEVICE` peripheral in async mode and split into RX/TX halves.
    pub fn new(usb_device: USB_DEVICE<'static>) -> Self {
        let (rx, tx) = UsbSerialJtag::new(usb_device).into_async().split();
        Self { rx, tx }
    }

    /// Read the next available bytes from the USB serial port.
    pub async fn read(&mut self, buf: &mut [u8]) -> usize {
        self.rx.read(buf).await.unwrap_or(0)
    }

    /// Write all bytes in `data` to the USB serial port and flush.
    pub async fn write_all(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let _ = self.tx.write_all(data).await;
        let _ = self.tx.flush().await;
    }
}
