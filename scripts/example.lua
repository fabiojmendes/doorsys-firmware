---@diagnostic disable: undefined-global
--- Example script for automating the configuration
--- of a new device. Update the tables for wifi and mqtt.
---
--- Run it using tio:
--- tio --script-file example.lua /dev/tty.USBxyz

local timeout = 2000

local wifi = {
	ssid = "ssid",
	pass = "password",
}

local mqtt = {
	url = "mqtt://example.com",
	user = "username",
	pass = "password",
}

write("\n")
if expect("doorsys>", timeout) ~= 1 then
	exit(1)
end

write(string.format("wifi %s %s\n", wifi.ssid, wifi.pass))
if expect("Wi-Fi configuration saved", timeout) ~= 1 then
	exit(1)
end

write("mqtt doorsys-dev.vm.lan user pass\n")
write(string.format("mqtt %s %s %s\n", mqtt.url, mqtt.user, mqtt.pass))
if expect("MQTT configuration saved", timeout) ~= 1 then
	exit(1)
end

write("reboot\n")
