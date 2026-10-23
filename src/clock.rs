//! Reusable SNTP wall-clock synchronization over `embassy-net`.
//!
//! Periodically queries an NTP server (`pool.ntp.org` by default) and computes an offset from
//! [`embassy_time::Instant`] so any task can query the current Unix timestamp without blocking.

use core::{
    cell::Cell,
    fmt,
    net::{IpAddr, SocketAddr},
};

use critical_section::Mutex;
use defmt::{info, warn};
use embassy_futures::select::{Either, select};
use embassy_net::{
    IpAddress, Stack,
    dns::DnsQueryType,
    udp::{PacketMetadata, UdpSocket},
};
use embassy_time::{Duration, Instant, Timer};
use sntpc::{NtpContext, NtpTimestampGenerator, get_time};
use sntpc_net_embassy::UdpSocketWrapper;

const DEFAULT_NTP_HOST: &str = "pool.ntp.org";
const NTP_PORT: u16 = 123;
const RESYNC_INTERVAL: Duration = Duration::from_secs(3600);
const RETRY_INTERVAL: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Stores the Unix timestamp in microseconds corresponding to `Instant::from_ticks(0)`.
static EPOCH_AT_BOOT_US: Mutex<Cell<Option<u64>>> = Mutex::new(Cell::new(None));

#[derive(Clone, Copy, Default)]
struct EmbassyTimestampGen {
    sec: u64,
    subsec_us: u32,
}

impl NtpTimestampGenerator for EmbassyTimestampGen {
    fn init(&mut self) {
        let us = current_unix_micros().unwrap_or_else(|| Instant::now().as_micros());
        self.sec = us / 1_000_000;
        self.subsec_us = (us % 1_000_000) as u32;
    }

    fn timestamp_sec(&self) -> u64 {
        self.sec
    }

    fn timestamp_subsec_micros(&self) -> u32 {
        self.subsec_us
    }
}

fn current_unix_micros() -> Option<u64> {
    let base = critical_section::with(|cs| EPOCH_AT_BOOT_US.borrow(cs).get())?;
    Some(base.saturating_add(Instant::now().as_micros()))
}

/// Returns `true` once SNTP has synchronized the wall clock at least once.
pub fn is_synced() -> bool {
    critical_section::with(|cs| EPOCH_AT_BOOT_US.borrow(cs).get().is_some())
}

/// Returns the current Unix timestamp in seconds (or `0` if SNTP has not yet synchronized).
pub fn now_unix_secs() -> u64 {
    current_unix_micros().map_or(0, |us| us / 1_000_000)
}

/// Returns the current Unix timestamp in nanoseconds (or uptime in nanoseconds before SNTP sync).
pub fn now_nanos() -> u128 {
    let us = current_unix_micros().unwrap_or_else(|| Instant::now().as_micros());
    u128::from(us) * 1_000
}

/// Returns the system uptime in seconds since boot.
pub fn uptime_secs() -> u64 {
    Instant::now().as_secs()
}

/// Returns a displayable [`Uptime`] value representing the current time since boot.
pub fn uptime() -> Uptime {
    Uptime(uptime_secs())
}

/// Human-readable uptime duration (`HHh MMm SSs` or `Dd HHh MMm SSs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uptime(pub u64);

impl fmt::Display for Uptime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.0 / 86_400;
        let hours = (self.0 % 86_400) / 3_600;
        let mins = (self.0 % 3_600) / 60;
        let secs = self.0 % 60;
        if days > 0 {
            write!(f, "{days}d {hours:02}h {mins:02}m {secs:02}s")
        } else {
            write!(f, "{hours:02}h {mins:02}m {secs:02}s")
        }
    }
}

impl defmt::Format for Uptime {
    fn format(&self, f: defmt::Formatter) {
        let days = self.0 / 86_400;
        let hours = (self.0 % 86_400) / 3_600;
        let mins = (self.0 % 3_600) / 60;
        let secs = self.0 % 60;
        if days > 0 {
            defmt::write!(
                f,
                "{=u64}d {=u64:02}h {=u64:02}m {=u64:02}s",
                days,
                hours,
                mins,
                secs
            );
        } else {
            defmt::write!(f, "{=u64:02}h {=u64:02}m {=u64:02}s", hours, mins, secs);
        }
    }
}

/// Continuously synchronize the wall clock against `DEFAULT_NTP_HOST`.
pub async fn run(stack: Stack<'static>) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buf = [0u8; 128];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_buf = [0u8; 128];

    loop {
        stack.wait_config_up().await;

        let addr = match stack.dns_query(DEFAULT_NTP_HOST, DnsQueryType::A).await {
            Ok(addrs) if !addrs.is_empty() => match addrs[0] {
                IpAddress::Ipv4(v4) => SocketAddr::new(IpAddr::V4(v4), NTP_PORT),
            },
            Ok(_) => {
                warn!("SNTP DNS returned no addresses");
                Timer::after(RETRY_INTERVAL).await;
                continue;
            }
            Err(e) => {
                warn!("SNTP DNS query failed: {:?}", e);
                Timer::after(RETRY_INTERVAL).await;
                continue;
            }
        };

        let mut udp = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
        if let Err(e) = udp.bind(0) {
            warn!("SNTP UDP bind failed: {:?}", e);
            Timer::after(RETRY_INTERVAL).await;
            continue;
        }

        let wrapper = UdpSocketWrapper::new(udp);
        let ctx = NtpContext::new(EmbassyTimestampGen::default());

        match select(get_time(addr, &wrapper, ctx), Timer::after(REQUEST_TIMEOUT)).await {
            Either::First(Ok(res)) => {
                let now_uptime_us = Instant::now().as_micros();
                let ntp_us = res.sec() * 1_000_000
                    + u64::from(sntpc::fraction_to_microseconds(res.sec_fraction()));
                let base_us = ntp_us.saturating_sub(now_uptime_us);
                critical_section::with(|cs| EPOCH_AT_BOOT_US.borrow(cs).set(Some(base_us)));
                info!("SNTP synchronized: unix_secs={}", res.sec());
                Timer::after(RESYNC_INTERVAL).await;
            }
            Either::First(Err(e)) => {
                warn!("SNTP request failed: {:?}", e);
                Timer::after(RETRY_INTERVAL).await;
            }
            Either::Second(()) => {
                warn!("SNTP request timed out");
                Timer::after(RETRY_INTERVAL).await;
            }
        }
    }
}
