<!-- vim: set tw=80: -->

# Doorsys Firmware

Doorsys is a door access control system for the esp32-c3 microcontroller.
Check [Autosys](https://github.com/fabiojmendes/autosys) for an overview
for the rest of the components on the platform.

This firmware is designed around the idea of multiple independent tasks that
communicate using [mpsc](https://doc.rust-lang.org/std/sync/mpsc/index.html).

It's main responsibilities are:

- Activate the relay circuit to open the door on a successful input
- Read user input from the wiegand reader and determine if the code is valid or
  not
- Send audit information via mqtt of all entry attempts
- Update it's internal database of valid codes based on messages received from
the mqtt broker
- Send health checks for observability

## Necessary Tooling

Install the necessary tooling as described in the [prerequisites section](https://github.com/esp-rs/esp-idf-template#prerequisites)
of the esp-idf-template. You can opt for the RISC-V alternative path as this
project is meant to run on a esp32-c3 microcontroller.

## To flash the firmware

```shell
# Optionally wipe flash before starting
espflash erase-flash --port /dev/port

espflash flash --port /dev/port doorsys-firmware-<version>.elf
```

## Initial Configuration

On first launch doorsys, will need to be provisioned with configurations for the
wifi network and also an mqtt server to communicate with.

Create a toml configuration file using the example bellow as reference. For
different auth methods check this
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

Upload a configuration file by connecting
to the default hotspot eg. `ESP_AABBCC` on port 23

```shell
nc -w1 192.168.71.1 23 < config.toml
```

## Reset to Factory

To reset the device configuration execute

```shell
espflash erase-region --port /dev/port 0x9000 0x6000
```

This command will erase the nvs partition and wipe all configs. On next reboot
doorsys will restart AP mode so you can follow the steps for [Initial
Configuration](#initial-configuration)
