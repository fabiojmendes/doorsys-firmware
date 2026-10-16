//! Reusable Wi-Fi station manager and `embassy-net` network stack initializer.
//!
//! Handles station configuration, DHCP with hostname registration, automatic reconnection on link
//! loss, and generating a unique `<prefix>-<mac_tail>` network identifier.

use core::{
    fmt::Write as _,
    sync::atomic::{AtomicBool, Ordering},
};

use defmt::{error, info, warn};
use embassy_net::{Config as NetConfig, DhcpConfig, Runner, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_hal::{peripherals::WIFI, rng::Rng};
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config as WifiModeConfig, Interface, Password, Ssid,
    WifiController, WifiError, sta::StationConfig,
};
use serde::{Deserialize, Serialize};

const RECONNECT_COOLDOWN: Duration = Duration::from_secs(5);

static WIFI_CONNECTED: AtomicBool = AtomicBool::new(false);

/// Returns `true` if the Wi-Fi station is currently associated with an access point.
pub fn is_connected() -> bool {
    WIFI_CONNECTED.load(Ordering::Relaxed)
}

/// Serializable Wi-Fi authentication method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, defmt::Format)]
pub enum AuthMethod {
    Open,
    Wep,
    Wpa,
    Wpa2Personal,
    WpaWpa2Personal,
}

impl AuthMethod {
    /// Parse an authentication method name from CLI input.
    pub fn parse(input: &str) -> Option<Self> {
        if input.eq_ignore_ascii_case("open") || input.eq_ignore_ascii_case("none") {
            Some(Self::Open)
        } else if input.eq_ignore_ascii_case("wep") {
            Some(Self::Wep)
        } else if input.eq_ignore_ascii_case("wpa") {
            Some(Self::Wpa)
        } else if input.eq_ignore_ascii_case("wpa2") || input.eq_ignore_ascii_case("wpa2personal") {
            Some(Self::Wpa2Personal)
        } else if input.eq_ignore_ascii_case("wpawpa2")
            || input.eq_ignore_ascii_case("wpa2wpa3")
            || input.eq_ignore_ascii_case("wpawpa2personal")
        {
            Some(Self::WpaWpa2Personal)
        } else {
            None
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Wep => "wep",
            Self::Wpa => "wpa",
            Self::Wpa2Personal => "wpa2",
            Self::WpaWpa2Personal => "wpawpa2",
        }
    }
}

/// Station credentials used to initialize the Wi-Fi controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, defmt::Format)]
pub struct WifiCredentials {
    pub ssid: heapless::String<32>,
    pub password: heapless::String<64>,
    pub auth: AuthMethod,
}

impl WifiCredentials {
    pub fn to_station_config(&self) -> Result<StationConfig, WifiError> {
        let ssid: Ssid = self.ssid.as_str().try_into()?;
        let pass: Password = self.password.as_str().try_into()?;
        let auth = match self.auth {
            AuthMethod::Open => AuthenticationMethodConfig::Open,
            AuthMethod::Wep => AuthenticationMethodConfig::Wep(pass),
            AuthMethod::Wpa => AuthenticationMethodConfig::Wpa(pass),
            AuthMethod::Wpa2Personal => AuthenticationMethodConfig::Wpa2Personal(pass),
            AuthMethod::WpaWpa2Personal => AuthenticationMethodConfig::WpaWpa2Personal(pass),
        };
        Ok(StationConfig::default()
            .with_ssid(ssid)
            .with_authentication(auth))
    }
}

/// Active network stack and station MAC address.
#[derive(Clone, Copy)]
pub struct WifiStack {
    pub stack: Stack<'static>,
    pub mac: [u8; 6],
}

/// Background runner maintaining the Wi-Fi station association.
pub struct WifiRunner {
    controller: WifiController<'static>,
}

impl WifiRunner {
    /// Connect to the configured access point and automatically reconnect when the link drops.
    pub async fn run(mut self) -> ! {
        loop {
            info!("Connecting to Wi-Fi...");
            match self.controller.connect_async().await {
                Ok(info) => {
                    WIFI_CONNECTED.store(true, Ordering::Relaxed);
                    info!(
                        "Wi-Fi connected to SSID {:?} (channel {})",
                        info.ssid, info.channel
                    );
                    let disc = self.controller.wait_for_disconnect_async().await;
                    WIFI_CONNECTED.store(false, Ordering::Relaxed);
                    warn!("Wi-Fi disconnected ({:?}), reconnecting...", disc);
                }
                Err(e) => {
                    WIFI_CONNECTED.store(false, Ordering::Relaxed);
                    error!("Wi-Fi connect error: {:?}", e);
                }
            }
            Timer::after(RECONNECT_COOLDOWN).await;
        }
    }
}

/// Initialize the Wi-Fi station interface and `embassy-net` TCP/IP stack.
pub fn init<const N: usize>(
    wifi_peripheral: WIFI<'static>,
    rng: Rng,
    creds: &WifiCredentials,
    hostname: &'static str,
    resources: &'static mut StackResources<N>,
) -> Result<(WifiStack, WifiRunner, Runner<'static, Interface>), WifiError> {
    let mut controller = WifiController::new(wifi_peripheral, Default::default())?;
    let sta_config = creds.to_station_config()?;
    controller.set_config(&WifiModeConfig::Station(sta_config))?;

    let interface = Interface::station();
    let mac = interface.mac_address();

    let mut dhcp = DhcpConfig::default();
    dhcp.hostname = heapless::String::try_from(hostname).ok();
    let net_config = NetConfig::dhcpv4(dhcp);

    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    let (stack, net_runner) = embassy_net::new(interface, net_config, resources, seed);

    Ok((
        WifiStack { stack, mac },
        WifiRunner { controller },
        net_runner,
    ))
}

/// Create a unique device identifier from `prefix` and the last 3 octets of `mac`
/// (e.g. `doorsys-a1b2c3`), matching the original firmware's `create_net_id`.
pub fn net_id(prefix: &str, mac: [u8; 6]) -> heapless::String<32> {
    let mac_id = mac
        .iter()
        .skip(3)
        .rev()
        .enumerate()
        .fold(0u32, |acc, (i, &x)| acc + (u32::from(x) << (i * 8)));
    let mut out = heapless::String::new();
    let _ = write!(out, "{}-{:x}", prefix, mac_id);
    out
}
