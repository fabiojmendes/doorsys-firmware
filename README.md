<!-- vim: set tw=80: -->

# Doorsys Firmware

Doorsys is a door access control system for the esp32-c3 microcontroller. Check
[Autosys](https://github.com/fabiojmendes/autosys) for an overview for the rest
of the components on the platform.

This firmware is designed around the idea of multiple independent tasks that
communicate using [mpsc](https://doc.rust-lang.org/std/sync/mpsc/index.html).

Its main responsibilities are:

- Activate the relay circuit to open the door on a successful input
- Read user input from the wiegand reader and determine if the code is valid or
  not
- Send audit information via MQTT of all entry attempts
- Update its internal database of valid codes based on messages received from
  the MQTT broker
- Send health checks for observability
- Keep the time up-to-date using NTP in order to keep audit logs consistent

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
