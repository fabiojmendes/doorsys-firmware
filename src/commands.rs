//! USB shell command definitions and async execution handlers.

use core::{
    fmt::Write as _,
    sync::atomic::{AtomicBool, Ordering},
};

use embedded_cli::{Command, cli::CliBuilder};
use esp_hal::peripherals::USB_DEVICE;

use crate::{
    clock,
    config::{self, MqttConfig},
    door, mqtt,
    shell::{BufferedWriter, UsbShellIo},
    storage::SharedStorage,
    user_db::UserDb,
    wifi::{self, AuthMethod, WifiCredentials, WifiStack},
};

static PENDING_REBOOT: AtomicBool = AtomicBool::new(false);

/// Top-level CLI commands available over the USB-Serial-JTAG shell.
#[derive(Command)]
enum CliCmd<'a> {
    /// Show device ID, firmware version, Wi-Fi/IP, MQTT, time sync, heap, and code count
    Status,

    /// Show stored Wi-Fi and MQTT configuration (passwords masked)
    Config,

    /// Configure Wi-Fi station credentials: wifi <ssid> <password> [--auth wpa2|wpawpa2|wpa|wep|open]
    Wifi {
        /// Wireless network SSID
        ssid: &'a str,
        /// Wireless network password (use "" for open networks)
        password: &'a str,
        /// Authentication method (default: wpa2)
        #[arg(long)]
        auth: Option<&'a str>,
    },

    /// Configure MQTT broker: mqtt <mqtt://host[:port]> <username> <password>
    Mqtt {
        /// Broker URL, e.g. mqtt://mqtt.example.com:1883
        url: &'a str,
        /// Broker username
        username: &'a str,
        /// Broker password
        password: &'a str,
    },

    /// Inspect or modify the local access code database
    Codes {
        #[command(subcommand)]
        op: CodesCmd,
    },

    /// Pulse the door relay open for 4 seconds
    Open,

    /// Restart the microcontroller
    Reboot,

    /// Erase all stored configuration and codes, then restart (requires --confirm)
    FactoryReset {
        /// Confirm destructive factory reset
        #[arg(long)]
        confirm: bool,
    },
}

/// Subcommands for `codes`.
#[derive(Command, Clone, Copy)]
enum CodesCmd {
    /// List all stored access codes
    List,
    /// Show the number of stored access codes
    Count,
    /// Add an access code to flash
    Add {
        /// Numeric PIN or RFID code
        code: i32,
    },
    /// Delete an access code from flash
    Del {
        /// Numeric PIN or RFID code
        code: i32,
    },
}

/// Owned command representation extracted from `embedded-cli`'s synchronous callback so we can
/// execute async flash/network operations outside the callback.
enum OwnedCmd {
    Status,
    Config,
    Wifi {
        ssid: heapless::String<32>,
        password: heapless::String<64>,
        auth: Option<heapless::String<16>>,
    },
    Mqtt {
        url: heapless::String<96>,
        username: heapless::String<64>,
        password: heapless::String<64>,
    },
    Codes(CodesCmd),
    Open,
    Reboot,
    FactoryReset {
        confirm: bool,
    },
    ArgumentTooLong(&'static str),
}

impl OwnedCmd {
    fn from_borrowed(cmd: CliCmd<'_>) -> Self {
        match cmd {
            CliCmd::Status => Self::Status,
            CliCmd::Config => Self::Config,
            CliCmd::Wifi {
                ssid,
                password,
                auth,
            } => {
                let Ok(ssid) = heapless::String::try_from(ssid) else {
                    return Self::ArgumentTooLong("ssid (max 32 chars)");
                };
                let Ok(password) = heapless::String::try_from(password) else {
                    return Self::ArgumentTooLong("password (max 64 chars)");
                };
                let auth = match auth {
                    Some(a) => match heapless::String::try_from(a) {
                        Ok(s) => Some(s),
                        Err(_) => return Self::ArgumentTooLong("auth"),
                    },
                    None => None,
                };
                Self::Wifi {
                    ssid,
                    password,
                    auth,
                }
            }
            CliCmd::Mqtt {
                url,
                username,
                password,
            } => {
                let Ok(url) = heapless::String::try_from(url) else {
                    return Self::ArgumentTooLong("url (max 96 chars)");
                };
                let Ok(username) = heapless::String::try_from(username) else {
                    return Self::ArgumentTooLong("username (max 64 chars)");
                };
                let Ok(password) = heapless::String::try_from(password) else {
                    return Self::ArgumentTooLong("password (max 64 chars)");
                };
                Self::Mqtt {
                    url,
                    username,
                    password,
                }
            }
            CliCmd::Codes { op } => Self::Codes(op),
            CliCmd::Open => Self::Open,
            CliCmd::Reboot => Self::Reboot,
            CliCmd::FactoryReset { confirm } => Self::FactoryReset { confirm },
        }
    }
}

/// Runtime context passed to the USB shell task.
pub struct ShellContext {
    pub device_id: heapless::String<32>,
    pub version: &'static str,
    pub git_version: &'static str,
    pub storage: &'static SharedStorage,
    pub user_db: UserDb,
    pub net: Option<WifiStack>,
}

/// Run the interactive USB-Serial-JTAG configuration shell forever.
pub async fn run_shell(usb_device: USB_DEVICE<'static>, ctx: ShellContext) -> ! {
    let mut io = UsbShellIo::new(usb_device);
    let mut cmd_buf = [0u8; 192];
    let mut hist_buf = [0u8; 256];
    let tx_buf = core::cell::RefCell::new(alloc::vec::Vec::with_capacity(512));

    let mut cli = CliBuilder::default()
        .writer(BufferedWriter::new(&tx_buf))
        .command_buffer(cmd_buf.as_mut_slice())
        .history_buffer(hist_buf.as_mut_slice())
        .prompt("doorsys> ")
        .build()
        .expect("Failed to build CLI");

    let mut rx_chunk = [0u8; 64];

    loop {
        flush_cli_writer(&tx_buf, &mut io).await;

        let n = io.read(&mut rx_chunk).await;
        for &byte in &rx_chunk[..n] {
            let mut captured: Option<OwnedCmd> = None;
            let _ = cli.process_byte::<CliCmd<'_>, _>(
                byte,
                &mut CliCmd::processor(|_handle, command| {
                    captured = Some(OwnedCmd::from_borrowed(command));
                    Ok(())
                }),
            );

            flush_cli_writer(&tx_buf, &mut io).await;

            if let Some(cmd) = captured {
                let mut out = heapless::String::<512>::new();
                execute_command(cmd, &ctx, &mut io, &mut out).await;
                if !out.is_empty() {
                    io.write_all(out.as_bytes()).await;
                }
            }
        }
    }
}

async fn flush_cli_writer(tx_buf: &core::cell::RefCell<alloc::vec::Vec<u8>>, io: &mut UsbShellIo) {
    let pending: alloc::vec::Vec<u8> = {
        let mut guard = tx_buf.borrow_mut();
        if guard.is_empty() {
            return;
        }
        core::mem::take(&mut *guard)
    };
    io.write_all(&pending).await;
}

async fn execute_command(
    cmd: OwnedCmd,
    ctx: &ShellContext,
    io: &mut UsbShellIo,
    out: &mut heapless::String<512>,
) {
    match cmd {
        OwnedCmd::ArgumentTooLong(field) => {
            let _ = writeln!(out, "\r\nError: {} is too long\r", field);
        }
        OwnedCmd::Status => {
            let stats = esp_alloc::HEAP.stats();
            let free = stats.size.saturating_sub(stats.current_usage);
            let codes = ctx.user_db.count().await.unwrap_or(0);

            let _ = writeln!(out, "\r\nDevice ID : {}\r", ctx.device_id);
            let _ = writeln!(out, "Firmware  : {} ({})\r", ctx.version, ctx.git_version);
            let _ = writeln!(
                out,
                "Wi-Fi     : {}\r",
                if wifi::is_connected() {
                    "connected"
                } else if ctx.net.is_some() {
                    "disconnected"
                } else {
                    "not configured"
                }
            );
            if let Some(net) = &ctx.net {
                if let Some(v4) = net.stack.config_v4() {
                    let _ = writeln!(out, "IPv4      : {}\r", v4.address);
                } else {
                    let _ = writeln!(out, "IPv4      : acquiring DHCP...\r");
                }
            }
            let _ = writeln!(
                out,
                "MQTT      : {}\r",
                if mqtt::is_connected() {
                    "connected"
                } else {
                    "disconnected"
                }
            );
            if clock::is_synced() {
                let _ = writeln!(out, "Time      : synced ({}s)\r", clock::now_unix_secs());
            } else {
                let _ = writeln!(out, "Time      : not synced\r");
            }
            let _ = writeln!(
                out,
                "Heap      : {} free / {} total (max used {})\r",
                free, stats.size, stats.max_usage
            );
            let _ = writeln!(out, "Codes     : {}\r", codes);
            if PENDING_REBOOT.load(Ordering::Relaxed) {
                let _ = writeln!(
                    out,
                    "Notice    : configuration changed; run 'reboot' to apply\r"
                );
            }
        }
        OwnedCmd::Config => {
            let _ = writeln!(out, "\r");
            match config::load_wifi(ctx.storage).await {
                Some(w) => {
                    let masked = if w.password.is_empty() {
                        "<empty>"
                    } else {
                        "********"
                    };
                    let _ = writeln!(
                        out,
                        "[wifi]\r\n  ssid     = {}\r\n  password = {}\r\n  auth     = {}\r",
                        w.ssid,
                        masked,
                        w.auth.as_str()
                    );
                }
                None => {
                    let _ = writeln!(out, "[wifi]\r\n  <not configured>\r");
                }
            }
            match config::load_mqtt(ctx.storage).await {
                Some(m) => {
                    let masked = if m.password.is_empty() {
                        "<empty>"
                    } else {
                        "********"
                    };
                    let _ = writeln!(
                        out,
                        "[mqtt]\r\n  url      = {}\r\n  username = {}\r\n  password = {}\r",
                        m.url, m.username, masked
                    );
                }
                None => {
                    let _ = writeln!(out, "[mqtt]\r\n  <not configured>\r");
                }
            }
        }
        OwnedCmd::Wifi {
            ssid,
            password,
            auth,
        } => {
            let auth_method = match auth.as_deref() {
                Some(a) => match AuthMethod::parse(a) {
                    Some(m) => m,
                    None => {
                        let _ = writeln!(
                            out,
                            "\r\nError: invalid auth '{}'. Expected wpa2, wpawpa2, wpa, wep, or open\r",
                            a
                        );
                        return;
                    }
                },
                None => AuthMethod::Wpa2Personal,
            };

            let creds = WifiCredentials {
                ssid,
                password,
                auth: auth_method,
            };
            if creds.to_station_config().is_err() {
                let _ = writeln!(out, "\r\nError: invalid SSID or password length\r");
                return;
            }

            match config::save_wifi(ctx.storage, &creds).await {
                Ok(()) => {
                    PENDING_REBOOT.store(true, Ordering::Relaxed);
                    let _ = writeln!(
                        out,
                        "\r\nWi-Fi configuration saved. Run 'reboot' to apply.\r"
                    );
                }
                Err(e) => {
                    let _ = writeln!(out, "\r\nError saving Wi-Fi config: {:?}\r", e);
                }
            }
        }
        OwnedCmd::Mqtt {
            url,
            username,
            password,
        } => {
            if config::parse_mqtt_url(url.as_str()).is_none() {
                let _ = writeln!(
                    out,
                    "\r\nError: invalid MQTT URL '{}'. Expected mqtt://host[:port]\r",
                    url
                );
                return;
            }
            let cfg = MqttConfig {
                url,
                username,
                password,
            };
            match config::save_mqtt(ctx.storage, &cfg).await {
                Ok(()) => {
                    PENDING_REBOOT.store(true, Ordering::Relaxed);
                    let _ = writeln!(
                        out,
                        "\r\nMQTT configuration saved. Run 'reboot' to apply.\r"
                    );
                }
                Err(e) => {
                    let _ = writeln!(out, "\r\nError saving MQTT config: {:?}\r", e);
                }
            }
        }
        OwnedCmd::Codes(sub) => match sub {
            CodesCmd::Count => match ctx.user_db.count().await {
                Ok(n) => {
                    let _ = writeln!(out, "\r\nStored codes: {}\r", n);
                }
                Err(e) => {
                    let _ = writeln!(out, "\r\nError counting codes: {:?}\r", e);
                }
            },
            CodesCmd::List => match ctx.user_db.list().await {
                Ok(codes) => {
                    let mut line = heapless::String::<64>::new();
                    let _ = writeln!(line, "\r\nTotal codes: {}\r", codes.len());
                    io.write_all(line.as_bytes()).await;
                    for c in codes {
                        line.clear();
                        let _ = writeln!(line, "  {}\r", c);
                        io.write_all(line.as_bytes()).await;
                    }
                }
                Err(e) => {
                    let _ = writeln!(out, "\r\nError listing codes: {:?}\r", e);
                }
            },
            CodesCmd::Add { code } => match ctx.user_db.add(code).await {
                Ok(()) => {
                    let _ = writeln!(out, "\r\nAdded code {}\r", code);
                }
                Err(e) => {
                    let _ = writeln!(out, "\r\nError adding code: {:?}\r", e);
                }
            },
            CodesCmd::Del { code } => match ctx.user_db.delete(code).await {
                Ok(()) => {
                    let _ = writeln!(out, "\r\nDeleted code {}\r", code);
                }
                Err(e) => {
                    let _ = writeln!(out, "\r\nError deleting code: {:?}\r", e);
                }
            },
        },
        OwnedCmd::Open => {
            door::trigger_open();
            let _ = writeln!(out, "\r\nDoor relay triggered.\r");
        }
        OwnedCmd::Reboot => {
            io.write_all(b"\r\nRebooting...\r\n").await;
            embassy_time::Timer::after(embassy_time::Duration::from_millis(100)).await;
            esp_hal::system::software_reset();
        }
        OwnedCmd::FactoryReset { confirm } => {
            if !confirm {
                let _ = writeln!(
                    out,
                    "\r\nRefusing factory reset without --confirm. Run 'factory-reset --confirm'.\r"
                );
                return;
            }
            match ctx.storage.lock().await.erase_all().await {
                Ok(()) => {
                    io.write_all(b"\r\nFlash erased. Rebooting...\r\n").await;
                    embassy_time::Timer::after(embassy_time::Duration::from_millis(100)).await;
                    esp_hal::system::software_reset();
                }
                Err(e) => {
                    let _ = writeln!(out, "\r\nError erasing flash: {:?}\r", e);
                }
            }
        }
    }
}
