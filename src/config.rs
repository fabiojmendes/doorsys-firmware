use core::str;
use std::io::{Read, Write};
use std::{ffi::OsString, net::TcpListener};

use anyhow::Context;
use esp_idf_svc::wifi::{BlockingWifi, ClientConfiguration, Configuration};
use esp_idf_svc::{
    hal::reset,
    nvs::{EspNvs, EspNvsPartition, NvsDefault},
    wifi::{AuthMethod, EspWifi},
};

use pico_args::Arguments;

const HELP: &str = "\
COMMANDS:
    
wifi -s <SSID> -p <PASSWORD> -a <AUTH>

    OPTIONS:
        -s, --ssid            Network SSID
        -p, --password        Password
        -a, --auth            Authentication method eg. WPA2Personal
            
mqtt -u <USER> -p <PASSWORD> URL

    OPTIONS:
        -u, --user            Mqtt broker username
        -p, --password        Mqtt broker password
    ARGS:
        <URL>                 Mqtt broker url

commit
    Apply configs and exit configuration mode

reboot
    Reboots the device

help
    Print this help information
";

enum Command {
    Help,
    Wifi {
        ssid: heapless::String<32>,
        password: heapless::String<64>,
        auth_method: AuthMethod,
    },
    Mqtt(MqttConfig),
    Commit,
    Reboot,
}

#[derive(Debug)]
pub struct MqttConfig {
    pub url: String,
    pub username: String,
    pub password: String,
}

pub struct DoorsysConfig {
    nvs: EspNvs<NvsDefault>,
}

impl DoorsysConfig {
    pub fn new(nvs_part: EspNvsPartition<NvsDefault>) -> anyhow::Result<Self> {
        Ok(DoorsysConfig {
            nvs: EspNvs::new(nvs_part, "config", true)?,
        })
    }

    pub fn read_mqtt_configs(&self) -> anyhow::Result<MqttConfig> {
        let mut buf = [0; 128];
        let url = String::from(
            self.nvs
                .get_str("mqtt_url", &mut buf)?
                .context("Mqtt url not set")?,
        );
        let username = String::from(
            self.nvs
                .get_str("mqtt_username", &mut buf)?
                .context("Mqtt username not set")?,
        );
        let password = String::from(
            self.nvs
                .get_str("mqtt_password", &mut buf)?
                .context("Mqtt password not set")?,
        );

        Ok(MqttConfig {
            url,
            username,
            password,
        })
    }

    pub fn run_config_server(&mut self, wifi: &mut BlockingWifi<EspWifi>) -> anyhow::Result<()> {
        let listener = TcpListener::bind("0.0.0.0:23")?;

        // accept connections and process them serially
        for stream in listener.incoming() {
            log::info!("New connection: {:?}", stream);
            let mut wifi_config = None;
            let mut mqtt_config = None;
            let mut stream = stream?;
            writeln!(stream, "Doorsys Configuration Console")?;
            loop {
                write!(stream, "$ ")?;
                let mut read = [0; 512];
                match stream.read(&mut read) {
                    Ok(n) => {
                        if n == 0 {
                            log::info!("Connection closed");
                            break;
                        }

                        let command_line = str::from_utf8(&read[0..n])?;
                        match parse_args(command_line) {
                            Ok(Command::Wifi {
                                ssid,
                                password,
                                auth_method,
                            }) => {
                                let config = ClientConfiguration {
                                    ssid,
                                    password,
                                    auth_method,
                                    ..Default::default()
                                };
                                wifi_config = Some(Configuration::Client(config));
                            }
                            Ok(Command::Mqtt(config)) => {
                                mqtt_config = Some(config);
                            }
                            Ok(Command::Reboot) => {
                                reset::restart();
                            }
                            Ok(Command::Commit) => {
                                if let Some(mqtt) = &mqtt_config {
                                    self.nvs.set_str("mqtt_url", &mqtt.url)?;
                                    self.nvs.set_str("mqtt_username", &mqtt.username)?;
                                    self.nvs.set_str("mqtt_password", &mqtt.password)?;
                                } else {
                                    writeln!(stream, "Missing mqtt configs")?;
                                    continue;
                                }
                                if let Some(config) = &wifi_config {
                                    writeln!(
                                        stream,
                                        "Applying configs, connection will be dropped"
                                    )?;
                                    log::info!("Applying configs and restarting wifi");
                                    wifi.stop()?;
                                    wifi.set_configuration(config)?;
                                    wifi.start()?;
                                    return Ok(());
                                } else {
                                    writeln!(stream, "Missing wifi configs")?;
                                    continue;
                                }
                            }
                            Ok(Command::Help) => {
                                writeln!(stream, "{}", HELP)?;
                            }
                            Err(parse_error) => {
                                writeln!(stream, "{}", parse_error)?;
                            }
                        }
                    }
                    Err(err) => {
                        log::error!("Error: {}", err);
                    }
                }
            }
        }
        log::info!("Closing config server");
        Ok(())
    }
}

fn parse_auth(auth_str: &str) -> Result<AuthMethod, pico_args::Error> {
    match auth_str {
        "None" => Ok(AuthMethod::None),
        "WEP" => Ok(AuthMethod::WEP),
        "WPA" => Ok(AuthMethod::WPA),
        "WPA2Personal" => Ok(AuthMethod::WPA2Personal),
        "WPAWPA2Personal" => Ok(AuthMethod::WPAWPA2Personal),
        "WPA2Enterprise" => Ok(AuthMethod::WPA2Enterprise),
        "WPA3Personal" => Ok(AuthMethod::WPA3Personal),
        "WPA2WPA3Personal" => Ok(AuthMethod::WPA2WPA3Personal),
        "WAPIPersonal" => Ok(AuthMethod::WAPIPersonal),

        _ => Err(pico_args::Error::Utf8ArgumentParsingFailed {
            value: String::from(auth_str),
            cause: String::from("Invalid auth method"),
        }),
    }
}

fn parse_args(command_line: &str) -> Result<Command, pico_args::Error> {
    let command_iter = command_line.split_whitespace();
    let args: Vec<OsString> = command_iter.map(|s| s.into()).collect();

    let mut pargs = Arguments::from_vec(args);
    match pargs.subcommand()?.as_deref() {
        Some("wifi") => {
            let ssid: String = pargs.value_from_str(["-s", "--ssid"])?;
            let password: String = pargs.value_from_str(["-p", "--password"])?;
            let auth_method = pargs.value_from_fn(["-a", "--auth"], parse_auth)?;
            Ok(Command::Wifi {
                ssid: ssid.as_str().try_into().unwrap(),
                password: password.as_str().try_into().unwrap(),
                auth_method,
            })
        }
        Some("mqtt") => Ok(Command::Mqtt(MqttConfig {
            username: pargs.value_from_str(["-u", "--username"])?,
            password: pargs.value_from_str(["-p", "--password"])?,
            url: pargs.free_from_str()?,
        })),
        Some("reboot") => Ok(Command::Reboot),
        Some("commit") => Ok(Command::Commit),
        Some("help") => Ok(Command::Help),
        Some(unknown) => Err(pico_args::Error::Utf8ArgumentParsingFailed {
            value: String::from(unknown),
            cause: String::from("Invalid command"),
        }),
        None => Err(pico_args::Error::MissingArgument),
    }
}
