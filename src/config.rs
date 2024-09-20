use core::str;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use esp_idf_svc::wifi::{BlockingWifi, ClientConfiguration, Configuration};
use esp_idf_svc::{
    nvs::{EspNvs, EspNvsPartition, NvsDefault},
    wifi::{AuthMethod, EspWifi},
};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Debug)]
struct Config {
    wifi: WifiConfig,
    mqtt: MqttConfig,
}

#[derive(Deserialize, Debug)]
struct WifiConfig {
    ssid: String,
    password: String,
    auth: AuthMethod,
}

#[derive(Serialize, Deserialize, Debug)]
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
        let mut buf = [0; 256];
        if let Ok(Some(slice)) = self.nvs.get_raw("mqtt", &mut buf) {
            let mqtt_config = postcard::from_bytes(slice)?;
            log::info!("bytes read for mqtt configuration");
            return Ok(mqtt_config);
        }
        anyhow::bail!("No mqtt config found");
    }

    pub fn run_config_server(&mut self, wifi: &mut BlockingWifi<EspWifi>) -> anyhow::Result<()> {
        let listener = TcpListener::bind("0.0.0.0:23")?;
        // accept connections and process them serially
        for stream_res in listener.incoming() {
            log::info!("New connection: {:?}", stream_res);
            match stream_res {
                Ok(mut stream) => {
                    if let Err(e) = self.apply_config(&mut stream, wifi) {
                        log::error!("Error parsing configuration: {}", e);
                        writeln!(stream, "Error parsing configuration {}", e)?;
                    } else {
                        // Close config server and continue with boot
                        break;
                    }
                }
                Err(e) => {
                    log::error!("Error: {}", e);
                }
            }
        }
        Ok(())
    }

    fn apply_config(
        &mut self,
        stream: &mut TcpStream,
        wifi: &mut BlockingWifi<EspWifi>,
    ) -> anyhow::Result<()> {
        let mut file = String::new();
        stream.read_to_string(&mut file)?;
        log::info!("New config\n{}", file);
        let config: Config = toml::from_str(&file)?;
        let payload = postcard::to_allocvec(&config.mqtt)?;
        self.nvs.set_raw("mqtt", &payload)?;

        let wifi_config = ClientConfiguration {
            ssid: config.wifi.ssid.as_str().try_into().unwrap(),
            password: config.wifi.password.as_str().try_into().unwrap(),
            auth_method: config.wifi.auth,
            ..Default::default()
        };
        writeln!(stream, "Success! Appying configs")?;
        wifi.stop()?;
        wifi.set_configuration(&Configuration::Client(wifi_config))?;
        wifi.start()?;
        Ok(())
    }
}
