<!-- vim: set tw=80: -->

# Doorsys Firmware

Doorsys is a door access control system for the esp32-c3 microcontroller. For an
overview of the other components in the platform, check out
[Autosys](https://github.com/fabiojmendes/autosys).

This firmware is designed around the concept of multiple independent tasks that
communicate via multi-producer, single-consumer
([mpsc](https://doc.rust-lang.org/std/sync/mpsc/index.html)) channels.

Its main responsibilities are:

- Activating the relay circuit to open the door upon successful input
- Reading user input from the Wiegand reader and validating the user code
- Sending audit information via MQTT for all entry attempts
- Updating its internal database of valid codes based on messages received from
  the MQTT broker
- Sending health checks for observability
- Keeping the time up-to-date using NTP to ensure consistent audit logs

## Necessary Tooling

Install the necessary tooling as described in the
[prerequisites section](https://github.com/esp-rs/esp-idf-template#prerequisites)
of the esp-idf-template. You can opt for the RISC-V alternative path as this
project is meant to run on an ESP32-C3 microcontroller.

## Flashing the Firmware

```shell
# Optionally wipe flash before starting
espflash erase-flash --port /dev/port

espflash flash --port /dev/port doorsys-firmware-<version>.elf
```

## Initial Configuration

On first launch Doorsys, will need to be provisioned with configurations for the
Wi-Fi network and also an MQTT server to communicate with.

Create a TOML configuration file using the example below as reference. For
different authentication methods check this
[enum](https://github.com/esp-rs/embedded-svc/blob/d4d86fcbc69f8a0a41b9ad735824c6ce22b1febe/src/wifi.rs#L28).

```toml
[wifi]
ssid = "MySSID"
password = "secret"
auth = "WPA2Personal"

[mqtt]
username = "username"
password = "password"
url = "mqtt://mqtt.example.com:1883"
```

Upload a configuration file by connecting to the default hotspot e.g.,
`ESP_AABBCC` on port 23

```shell
nc -w1 192.168.71.1 23 < config.toml
```

## Reset to Factory

To reset the device configuration execute

```shell
espflash erase-region --port /dev/port 0x9000 0x6000
```

This command will erase the NVS partition and wipe all configurations. On next
reboot Doorsys will restart AP mode so you can follow the steps for
[initial configuration](#initial-configuration).

## Usage

After completing the initial configuration, and making the connections as
described in the [hardware](https://github.com/fabiojmendes/doorsys-hardware),
the system should be ready to use.

To operate the door using the keypad, the user should enter the 6 digits pins
followed by a `#` key. Once a valid sequence is entered, the keypad will emit a
sound the relay will be activated for 4 seconds allowing the user to open the
door. Tapping a badge doesn't require a `#` press as it will automatically
validate the code. At any point the `*` key may be used to cancel an erroneous
input. If an invalid or incomplete pin is entered, a rapid intermittent sound
will be played notifying the user of the error. The same behavior is true for an
invalid badge.
