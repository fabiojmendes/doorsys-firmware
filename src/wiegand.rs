use core::ffi::c_void;
use std::{
    ffi::CString,
    ptr,
    sync::mpsc::{self, Receiver, Sender},
};

use esp_idf_svc::{
    hal::gpio::InputPin,
    sys::{
        esp, esp_timer_create, esp_timer_create_args_t, esp_timer_delete,
        esp_timer_dispatch_t_ESP_TIMER_TASK, esp_timer_handle_t, esp_timer_start_once,
        esp_timer_stop, gpio_config, gpio_config_t, gpio_get_level,
        gpio_int_type_t_GPIO_INTR_DISABLE, gpio_int_type_t_GPIO_INTR_NEGEDGE, gpio_isr_handler_add,
        gpio_isr_handler_remove, gpio_mode_t_GPIO_MODE_INPUT, gpio_reset_pin, gpio_set_intr_type,
    },
};

const WIEGAND_TIMEOUT: u64 = 50000; // 50ms
const BUFFER_SIZE: usize = 4;

#[link_section = ".iram0.text"]
unsafe extern "C" fn wiegand_interrupt<D0: InputPin, D1: InputPin>(arg: *mut c_void) {
    let reader = &mut *(arg as *mut Reader<D0, D1>);
    let d0 = gpio_get_level(reader.d0_gpio.pin());
    let d1 = gpio_get_level(reader.d1_gpio.pin());
    if d0 == d1 {
        return;
    }
    // Overflow
    if reader.bits > reader.data.len() * 8 {
        return;
    }

    esp_timer_stop(reader.timer);

    let value = if d0 == 0 { 0 } else { 0x80 };
    reader.data[reader.bits / 8] |= value >> (reader.bits % 8);
    reader.bits += 1;

    esp_timer_start_once(reader.timer, WIEGAND_TIMEOUT);
}

unsafe extern "C" fn timer_interrupt<D0: InputPin, D1: InputPin>(arg: *mut c_void) {
    let reader = &mut *(arg as *mut Reader<D0, D1>);
    reader.stop();

    let packet = Packet::new(reader.bits, reader.data);

    if let Err(e) = reader.reader_tx.send(packet) {
        log::error!("send error {}", e);
    }
    reader.reset();
}

/// Check parity bits 25 (even) and 0 (odd)
///
/// Reference:
/// https://getsafeandsound.com/blog/26-bit-wiegand-format/
/// Calculator
/// http://www.ccdesignworks.com/wiegand_calc.htm
fn parity_check_26bits(mut rfid: u32) -> bool {
    // Odd parity is checked over the rightmost 13 bits.
    let mut count = 0;
    for _ in 0..13 {
        count += rfid & 1;
        rfid >>= 1;
    }
    if count % 2 == 0 {
        return false;
    }

    // Even parity is checked over the leftmost 13 bits
    let mut count = 0;
    for _ in 0..13 {
        count += rfid & 1;
        rfid >>= 1;
    }
    if count % 2 == 1 {
        return false;
    }

    true
}

/// Packet read from the wiegand interface
/// It can be a card tap, a key press or undefined bits
#[derive(Debug)]
pub enum Packet {
    Key {
        key: u8,
    },
    Card {
        rfid: i32,
    },
    Unknown {
        bits: usize,
        data: [u8; BUFFER_SIZE],
    },
}

impl Packet {
    fn new(bits: usize, data: [u8; BUFFER_SIZE]) -> Self {
        log::info!("data received; bits: {}, data: {:02X?}", bits, data);
        match bits {
            4 => Self::Key { key: data[0] >> 4 },
            26 => {
                let mut rfid: u32 = (data[0] as u32) << 24
                    | (data[1] as u32) << 16
                    | (data[2] as u32) << 8
                    | (data[3] as u32);

                // Remove padding bits
                rfid >>= 6;

                if !parity_check_26bits(rfid) {
                    log::warn!("Parity check failed");
                    return Self::Unknown { bits, data };
                }

                // Remove partiy check bits
                rfid &= !(1 << 25);
                rfid >>= 1;

                let rfid = rfid as i32;

                Self::Card { rfid }
            }
            _ => Self::Unknown { bits, data },
        }
    }
}

/// Wiegand reader
/// This is the implementation the wiegand protocol using 2 gpio pins.
/// The interrupt service must be installed as this code relies on interrupts
/// to read the sinterface signals.
///
/// Usage:
/// ```rust
/// // Installs the generic GPIO interrupt handler
/// esp!(unsafe { gpio_install_isr_service(ESP_INTR_FLAG_IRAM as i32) })?;
///
/// let reader = Reader::new(d0, d1);
/// // init must be called before any interaction with the reader
/// reader.init();
/// for packet in reader {
///     // proccess packet
/// }
/// ```
pub struct Reader<D0: InputPin, D1: InputPin> {
    bits: usize,
    data: [u8; BUFFER_SIZE],
    d0_gpio: D0,
    d1_gpio: D1,
    timer: esp_timer_handle_t,
    reader_tx: Sender<Packet>,
}

impl<D0: InputPin, D1: InputPin> Reader<D0, D1> {
    pub fn new(d0_gpio: D0, d1_gpio: D1) -> (Self, Receiver<Packet>) {
        let (reader_tx, reader_rx) = mpsc::channel();
        (
            Reader {
                d0_gpio,
                d1_gpio,
                data: [0; BUFFER_SIZE],
                bits: 0,
                timer: ptr::null_mut(),
                reader_tx,
            },
            reader_rx,
        )
    }

    /// This implementation is a little messy and may contain UB.
    /// Ideally a fully initilized instance should be returned from the new
    /// function.
    ///
    /// Investigate a possible implementation using Pin
    pub fn init(&mut self) -> anyhow::Result<()> {
        let reader_ptr = self as *mut _ as *mut c_void;

        let timer_config = esp_timer_create_args_t {
            name: CString::new("wiegand")?.into_raw(),
            arg: reader_ptr,
            callback: Some(timer_interrupt::<D0, D1>),
            dispatch_method: esp_timer_dispatch_t_ESP_TIMER_TASK,
            skip_unhandled_events: true,
        };

        esp!(unsafe { esp_timer_create(&timer_config, &mut self.timer) })?;

        // Configures d0 and d1
        let io_conf = gpio_config_t {
            pin_bit_mask: (1 << self.d0_gpio.pin() | 1 << self.d1_gpio.pin()),
            mode: gpio_mode_t_GPIO_MODE_INPUT,
            pull_up_en: true.into(),
            pull_down_en: false.into(),
            intr_type: gpio_int_type_t_GPIO_INTR_NEGEDGE,
        };

        unsafe {
            // Writes the configuration to the registers
            esp!(gpio_config(&io_conf))?;

            // Registers our function with the generic GPIO interrupt handler
            // This assumes gpio_install_isr_service was called before
            esp!(gpio_isr_handler_add(
                self.d0_gpio.pin(),
                Some(wiegand_interrupt::<D0, D1>),
                reader_ptr
            ))?;
            esp!(gpio_isr_handler_add(
                self.d1_gpio.pin(),
                Some(wiegand_interrupt::<D0, D1>),
                reader_ptr
            ))?;
        }

        Ok(())
    }

    fn stop(&mut self) {
        unsafe {
            esp_timer_stop(self.timer);
            gpio_set_intr_type(self.d0_gpio.pin(), gpio_int_type_t_GPIO_INTR_DISABLE);
            gpio_set_intr_type(self.d1_gpio.pin(), gpio_int_type_t_GPIO_INTR_DISABLE);
        }
    }

    fn reset(&mut self) {
        unsafe {
            gpio_set_intr_type(self.d0_gpio.pin(), gpio_int_type_t_GPIO_INTR_NEGEDGE);
            gpio_set_intr_type(self.d1_gpio.pin(), gpio_int_type_t_GPIO_INTR_NEGEDGE);
        }
        self.data = [0; BUFFER_SIZE];
        self.bits = 0;
    }
}

impl<D0: InputPin, D1: InputPin> Drop for Reader<D0, D1> {
    fn drop(&mut self) {
        unsafe {
            esp_timer_stop(self.timer);
            esp_timer_delete(self.timer);

            gpio_isr_handler_remove(self.d0_gpio.pin());
            gpio_reset_pin(self.d0_gpio.pin());

            gpio_isr_handler_remove(self.d1_gpio.pin());
            gpio_reset_pin(self.d1_gpio.pin());
        }
    }
}
