# Doorsys Firmware

## To flash the firmware

```shell
# Optionally wipe flash before starting
espflash erase-flash --port /dev/port

espflash flash --port /dev/port doorsys-firmware-<version>.elf
```

## Initial configuration

Create a configuration file using the example bellow as reference

```toml
[wifi]
ssid = "MySSID"
password = "secret"
auth = "WPA2WPA3Personal"

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

This command will erase the nvs partition and wipe all configs
