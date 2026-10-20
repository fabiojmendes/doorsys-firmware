#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

use defmt::{error, info, warn};
use doorsys_firmware::{
    access, built_info, clock,
    commands::{self, ShellContext},
    config,
    door::Door,
    health, mk_static,
    mqtt::{self, MqttSettings},
    storage::{SharedStorage, Storage},
    user_db::UserDb,
    wiegand::Reader,
    wifi::{self, WifiRunner},
};
use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources};
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use esp_hal::{
    // clock::CpuClock,
    gpio::{DriveMode, Io, Level, Output, OutputConfig},
    peripherals::{TSENS, USB_DEVICE},
    rng::Rng,
    timer::timg::TimerGroup,
};
use esp_radio::wifi::Interface;
use panic_rtt_target as _;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

// Full `.dram2_uninit` region for this chip.
const DRAM2_UNINIT_SIZE: usize = {
    let range: core::ops::Range<usize> = esp_metadata_generated::memory_range!("DRAM2_UNINIT");
    range.end - range.start
};

const HOSTNAME_PREFIX: &str = "doorsys";
const USER_TOPIC: &str = "doorsys/user";

#[embassy_executor::task]
async fn door_task(relay_pin: Output<'static>) -> ! {
    Door::new(relay_pin).run().await
}

#[embassy_executor::task]
async fn access_task(reader: Reader, signal_pin: Output<'static>, user_db: UserDb) -> ! {
    access::run(reader, signal_pin, user_db).await
}

#[embassy_executor::task]
async fn shell_task(usb_device: USB_DEVICE<'static>, ctx: ShellContext) -> ! {
    commands::run_shell(usb_device, ctx).await
}

#[embassy_executor::task]
async fn wifi_task(runner: WifiRunner) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn sntp_task(stack: Stack<'static>) -> ! {
    clock::run(stack).await
}

#[embassy_executor::task]
async fn health_task(
    version: &'static str,
    storage: &'static SharedStorage,
    tsens: TSENS<'static>,
) -> ! {
    health::run(version, storage, tsens).await
}

#[allow(
    clippy::large_stack_frames,
    reason = "MQTT task holds TCP RX/TX buffers and protocol state across reconnects"
)]
#[embassy_executor::task]
async fn mqtt_task(stack: Stack<'static>, settings: MqttSettings, user_db: UserDb) -> ! {
    mqtt::run(stack, settings, USER_TOPIC, async move |topic, payload| {
        if topic == USER_TOPIC {
            user_db.process_user_message(payload).await;
        } else {
            warn!("Unhandled MQTT topic: {}", topic);
        }
    })
    .await
}

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    rtt_target::rtt_init_defmt!();

    // let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(esp_hal::Config::default());

    // The following pins are used to bootstrap the chip. They are available
    // for use, but check the datasheet of the module for more information on them.
    // - GPIO2
    // - GPIO8
    // - GPIO9
    // These GPIO pins are in use by some feature of the module and should not be used.
    let _gpio11 = peripherals.GPIO11;
    let _gpio12 = peripherals.GPIO12;
    let _gpio13 = peripherals.GPIO13;
    let _gpio14 = peripherals.GPIO14;
    let _gpio15 = peripherals.GPIO15;
    let _gpio16 = peripherals.GPIO16;
    let _gpio17 = peripherals.GPIO17;

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: DRAM2_UNINIT_SIZE);
    esp_alloc::heap_allocator!(size: 32 * 1024);
    info!("Heap allocation: {}", DRAM2_UNINIT_SIZE);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    let git_version = built_info::GIT_VERSION.unwrap_or("unknown");
    info!(
        "Doorsys version {}, built for {} by {} (git: {})",
        built_info::PKG_VERSION,
        built_info::TARGET,
        built_info::RUSTC_VERSION,
        git_version
    );

    let storage_inner = Storage::new(peripherals.FLASH)
        .await
        .expect("Failed to initialize flash storage");
    let storage = mk_static!(SharedStorage, Mutex::new(storage_inner));
    let user_db = UserDb::new(storage);

    let wifi_cfg = config::load_wifi(storage).await;
    let mqtt_cfg = config::load_mqtt(storage).await;

    // GPIO10: Door relay output (active high).
    let relay_output = Output::new(peripherals.GPIO10, Level::Low, OutputConfig::default());
    spawner.spawn(door_task(relay_output).expect("Failed to spawn door_task"));

    // GPIO4 (D0) & GPIO5 (D1): Wiegand inputs; GPIO7: keypad buzzer/LED open-drain output.
    let mut io = Io::new(peripherals.IO_MUX);
    let reader = Reader::new(&mut io, peripherals.GPIO4, peripherals.GPIO5);
    let signal_output = Output::new(
        peripherals.GPIO7,
        Level::High,
        OutputConfig::default().with_drive_mode(DriveMode::OpenDrain),
    );

    let mut device_id = heapless::String::<32>::try_from(HOSTNAME_PREFIX)
        .unwrap_or_else(|_| heapless::String::new());
    let mut net_stack = None;

    if let Some(ref creds) = wifi_cfg {
        let resources = mk_static!(StackResources<6>, StackResources::new());
        match wifi::init(
            peripherals.WIFI,
            Rng::new(),
            creds,
            HOSTNAME_PREFIX,
            resources,
        ) {
            Ok((wifi_stack, wifi_runner, net_runner)) => {
                device_id = wifi::net_id(HOSTNAME_PREFIX, wifi_stack.mac);
                info!("Device net_id: {}", device_id.as_str());
                net_stack = Some(wifi_stack);

                spawner.spawn(wifi_task(wifi_runner).expect("Failed to spawn wifi_task"));
                spawner.spawn(net_task(net_runner).expect("Failed to spawn net_task"));
                spawner.spawn(sntp_task(wifi_stack.stack).expect("Failed to spawn sntp_task"));
                spawner.spawn(
                    health_task(git_version, storage, peripherals.TSENS)
                        .expect("Failed to spawn health_task"),
                );

                if let Some(m) = mqtt_cfg {
                    if let Some((host, port)) = m.parse_endpoint() {
                        let settings = MqttSettings {
                            host,
                            port,
                            client_id: device_id.clone(),
                            username: m.username,
                            password: m.password,
                        };
                        spawner.spawn(
                            mqtt_task(wifi_stack.stack, settings, user_db)
                                .expect("Failed to spawn mqtt_task"),
                        );
                    } else {
                        error!(
                            "Invalid stored MQTT URL '{}'; use the USB shell `mqtt` command to fix",
                            m.url.as_str()
                        );
                    }
                } else {
                    warn!(
                        "No MQTT config found; running offline. Configure via USB shell `mqtt` command."
                    );
                }
            }
            Err(e) => {
                error!("Failed to initialize Wi-Fi: {:?}", e);
            }
        }
    } else {
        warn!(
            "No Wi-Fi config found; running offline. Connect over USB serial and use `wifi` and `mqtt` commands."
        );
    }

    spawner
        .spawn(access_task(reader, signal_output, user_db).expect("Failed to spawn access_task"));

    let shell_ctx = ShellContext {
        device_id,
        version: built_info::PKG_VERSION,
        git_version,
        storage,
        net: net_stack,
    };
    spawner
        .spawn(shell_task(peripherals.USB_DEVICE, shell_ctx).expect("Failed to spawn shell_task"));

    info!("Application initialized");

    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}
