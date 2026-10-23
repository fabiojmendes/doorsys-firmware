<!-- vim: set tw=80: -->

# Doorsys firmware

Doorsys is a `no_std` door access control firmware for the ESP32-C3
microcontroller built on [`esp-hal`](https://github.com/esp-rs/esp-hal) and the
[Embassy](https://embassy.dev/) async runtime. For an overview of the other
components in the platform, check out
[Autosys](https://github.com/fabiojmendes/autosys).

This firmware is designed around multiple cooperative Embassy tasks that
communicate via [`embassy-sync`](https://docs.rs/embassy-sync) channels and
signals.

Its main responsibilities are:

- Activating the relay circuit to open the door upon successful input
- Reading user input from the Wiegand reader (`GPIO4`/`GPIO5`) and validating
  the user code directly against flash storage
- Driving keypad buzzer/LED feedback (`GPIO7`, open-drain)
- Sending audit information via MQTT v5 for all entry attempts
- Updating its flash database of valid codes based on messages received from the
  MQTT broker (`doorsys/user`)
- Sending periodic heap and storage health metrics (`doorsys/status`)
- Keeping the wall clock synchronized via SNTP (`pool.ntp.org`)
- Providing an interactive USB-Serial-JTAG shell for provisioning and diagnostics

## Necessary tooling

1. Install the stable Rust toolchain with the `riscv32imc-unknown-none-elf`
   target and `rust-src` component (automatically selected by
   `rust-toolchain.toml`):

   ```shell
   rustup target add riscv32imc-unknown-none-elf
   rustup component add rust-src
   ```

2. Install [`probe-rs`](https://probe.rs/) (used as the default Cargo runner for
   flashing and `defmt` RTT logging) or [`espflash`](https://github.com/esp-rs/espflash).

## Building and flashing

```shell
# Build and flash with defmt logging via probe-rs
cargo run --release

# Or flash using espflash
espflash flash --port /dev/cu.usbmodemXXXX target/riscv32imc-unknown-none-elf/release/doorsys-firmware
```

## Initial configuration (USB shell)

Doorsys exposes an interactive command-line shell over the ESP32-C3's built-in
USB-Serial-JTAG CDC serial port. Connect to the serial port with any terminal
emulator (e.g. `tio`, `picocom`, or `screen`):

```shell
screen /dev/cu.usbmodemXXXX 115200
```

At the `doorsys>` prompt, configure Wi-Fi and MQTT and then reboot:

```text
doorsys> help
doorsys> wifi MySSID "my-secret-password" --auth wpa2
doorsys> mqtt mqtt://mqtt.example.com:1883 username password
doorsys> config
doorsys> reboot
```

Available shell commands:

- `status` — Show device ID, firmware version, Wi-Fi/IPv4 state, MQTT state,
  SNTP sync status, heap usage, and stored code count
- `config` — Show stored Wi-Fi and MQTT settings (with passwords masked)
- `wifi <ssid> <password> [--auth wpa2|wpawpa2|wpa|wep|open]` — Save Wi-Fi
  station credentials
- `mqtt <mqtt://host[:port]> <username> <password>` — Save MQTT broker settings
- `codes list | count | add <code> | del <code>` — Inspect or modify stored
  access codes
- `open` — Trigger the door relay for 4 seconds
- `reboot` — Restart the microcontroller
- `factory-reset --confirm` — Erase the storage partition and restart

## Reset to factory

From the USB shell:

```text
doorsys> factory-reset --confirm
```

Or using `espflash`:

```shell
espflash erase-region --port /dev/cu.usbmodemXXXX 0x9000 0x6000
```

## Usage

After completing the initial configuration and making the connections as
described in the [hardware](https://github.com/fabiojmendes/doorsys-hardware),
the system is ready to use.

To operate the door using the keypad, enter the PIN followed by the `#` key.
Once a valid sequence is entered, the keypad emits a continuous tone and the
relay (`GPIO10`) is activated for 4 seconds allowing the user to open the door.
Tapping an RFID badge does not require a `#` press as it automatically validates
the code. At any point the `*` key may be used to cancel an erroneous input. If
an invalid or incomplete PIN is entered, a rapid intermittent sound is played
notifying the user of the error. The same behavior applies to an invalid badge.

Once a user starts typing a PIN, they have 10 seconds to complete the sequence,
otherwise the operation is cancelled.
