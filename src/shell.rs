//! Reusable async USB-Serial-JTAG interactive shell built on `embedded-cli`.
//!
//! Bridges `embedded-cli`'s synchronous `embedded-io 0.6` writer interface to `esp-hal`'s async
//! `UsbSerialJtag` peripheral so the shell never blocks the Embassy executor while waiting for host
//! terminal reads or writes.

use core::{cell::RefCell, convert::Infallible};

use defmt::warn;
use embedded_io_async::{Read as AsyncRead, Write as AsyncWrite};
use esp_hal::{
    Async,
    peripherals::USB_DEVICE,
    usb::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx},
};

/// Fixed-capacity output buffer shared between [`BufferedWriter`] and the shell loop.
pub type TxBuffer<const N: usize> = RefCell<heapless::Vec<u8, N>>;

/// In-memory byte sink implementing `embedded_io_06::Write` for `embedded-cli`.
///
/// Output exceeding the buffer capacity `N` before the next [`UsbShellIo::drain`] is dropped.
pub struct BufferedWriter<'a, const N: usize> {
    buf: &'a TxBuffer<N>,
}

impl<'a, const N: usize> BufferedWriter<'a, N> {
    pub fn new(buf: &'a TxBuffer<N>) -> Self {
        Self { buf }
    }
}

// TODO: Replace `embedded_cli::__private::io` trait implementations once a new version of
// `embedded-cli` is released with `embedded-io` 0.7+ support.
impl<const N: usize> embedded_cli::__private::io::ErrorType for BufferedWriter<'_, N> {
    type Error = Infallible;
}

impl<const N: usize> embedded_cli::__private::io::Write for BufferedWriter<'_, N> {
    fn write(&mut self, data: &[u8]) -> Result<usize, Self::Error> {
        let mut out = self.buf.borrow_mut();
        let mut overflow = false;
        // Translate bare `\n` to `\r\n` for standard serial terminals when not already preceded by `\r`.
        for &b in data {
            if b == b'\n' && out.last().copied() != Some(b'\r') {
                overflow |= out.push(b'\r').is_err();
            }
            overflow |= out.push(b).is_err();
        }
        if overflow {
            warn!("Shell output buffer full ({} bytes); output truncated", N);
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

    /// Write out and clear everything accumulated in `buf`.
    ///
    /// Copies in small stack chunks so no `RefCell` borrow is held across an `.await`. Must only
    /// be called from the task that owns the [`BufferedWriter`] (nothing else may write to `buf`
    /// concurrently).
    pub async fn drain<const N: usize>(&mut self, buf: &TxBuffer<N>) {
        const CHUNK: usize = 64;
        let mut offset = 0;
        loop {
            let mut chunk = [0u8; CHUNK];
            let n = {
                let pending = buf.borrow();
                let rest = pending.get(offset..).unwrap_or(&[]);
                let n = rest.len().min(CHUNK);
                chunk[..n].copy_from_slice(&rest[..n]);
                n
            };
            if n == 0 {
                break;
            }
            let _ = self.tx.write_all(&chunk[..n]).await;
            offset += n;
        }
        if offset > 0 {
            buf.borrow_mut().clear();
            let _ = self.tx.flush().await;
        }
    }
}
