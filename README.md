# Doorsys Firmware

## To flash the firmware

```
# Optionally wipe flash before starting
espflash erase-flash --port /dev/port

espflash flash --port /dev/port doorsys-firmware-<version>.elf
```

## Initial configuration

Connect to the provided hotspot eg. `ESP_AABBCC` on port 23

```
nc 192.168.71.1 23
```
