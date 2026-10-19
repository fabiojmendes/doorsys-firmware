//! Reusable MQTT v5 client supervisor built on `minimq` and `embassy-net`.
//!
//! Manages DNS resolution, TCP connection, persistent MQTT v5 session recovery
//! (`ConfigBuilder::session_expiry_interval`), topic subscription, automatic keep-alive pings,
//! QoS 1 in-flight packet retention/replay, and separate outgoing queues for [`Audit`] records
//! and health telemetry.

use core::{
    fmt::Write as _,
    sync::atomic::{AtomicBool, Ordering},
};

use defmt::{error, info, warn};
use doorsys_protocol::Audit;
use embassy_futures::select::{Either3, select3};
use embassy_net::{IpEndpoint, Stack, dns::DnsQueryType, tcp::TcpSocket};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Duration, Timer};
use minimq::{
    Buffers, ConfigBuilder, ConnectEvent, Publication, QoS, RetainHandling, Session,
    SubscriptionOptions, TopicFilter,
};
use static_cell::ConstStaticCell;

// ============================================================================
// Tuning & Buffer Configuration Constants
// ============================================================================

/// Maximum number of [`Audit`] records queued in RAM while the device is offline or backpressured.
///
/// Each slot stores a 24-byte [`Audit`] struct in `.bss` (`128 * 24 = 3,072` bytes).
/// Increase this value (e.g. `256` = 6 KiB, `512` = 12 KiB) to retain more door access events
/// across extended Wi-Fi or broker outages.
pub const AUDIT_OUTBOX_CAPACITY: usize = 128;

/// Maximum number of formatted health telemetry lines queued in RAM while connected.
///
/// [`crate::health`] emits 2 status lines (`heap` and `storage`) every 60 seconds. Telemetry is
/// only enqueued when [`is_connected`] is `true`, so 2 slots (~520 bytes in `.bss`) is sufficient
/// without ever competing with [`AUDIT_OUTBOX`].
pub const TELEMETRY_OUTBOX_CAPACITY: usize = 2;

/// Maximum byte length of a single formatted health telemetry line on `doorsys/status`.
pub const MAX_TELEMETRY_LEN: usize = 256;

/// Size in bytes of the `minimq` inbound packet buffer (`rx`).
///
/// Holds one incoming MQTT control packet at a time and is advertised to the broker via the
/// MQTT v5 `MaximumPacketSize` property. Size this for the largest `UserAction::Bulk(Vec<i32>)`
/// payload expected (~4–5 bytes per user code in `postcard`; 4096 bytes fits ~800–1,000 codes).
pub const MQTT_RX_BUF_SIZE: usize = 4096;

/// Size in bytes of the `minimq` outbound transmit and replay arena (`tx`).
///
/// Used by `minimq` to:
/// - retain up to 8 unacknowledged in-flight QoS 1 [`Audit`] packets (~70 bytes each) for
///   automatic `DUP` replay across reconnects
/// - encode the `CONNECT` handshake (~200 bytes)
/// - provide temporary scratch space for one QoS 0 telemetry publish (~300 bytes)
pub const MQTT_TX_BUF_SIZE: usize = 2048;

/// Size in bytes of the underlying `embassy-net` TCP socket receive ring buffer.
pub const TCP_RX_BUF_SIZE: usize = 4096;

/// Size in bytes of the underlying `embassy-net` TCP socket transmit ring buffer.
pub const TCP_TX_BUF_SIZE: usize = 2048;

/// Broker-side MQTT v5 `SessionExpiryInterval` in seconds.
///
/// `u32::MAX` (`0xFFFF_FFFF`) instructs the broker to never expire the session on disconnect so
/// subscriptions and queued QoS 1 messages persist across network outages. Set to e.g. `86_400`
/// to expire broker session state after 24 hours offline.
pub const SESSION_EXPIRY_SECS: u32 = u32::MAX;

/// MQTT keep-alive interval in seconds advertised in `CONNECT`.
///
/// `minimq` automatically schedules `PINGREQ` and enforces `PINGRESP` timeouts inside
/// `Connection::recv` / `Connection::poll`.
pub const KEEP_ALIVE_SECS: u16 = 30;

/// Delay between reconnect attempts when DNS, TCP, or MQTT handshakes fail.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Maximum byte length of a formatted `doorsys/audit/<client_id>` topic string.
const MAX_AUDIT_TOPIC_LEN: usize = 64;

/// Topic used for periodic health telemetry publications.
const STATUS_TOPIC: &str = "doorsys/status";

// ============================================================================
// Static Buffers & Channels
// ============================================================================

static MQTT_CONNECTED: AtomicBool = AtomicBool::new(false);

static TCP_RX_BUF: ConstStaticCell<[u8; TCP_RX_BUF_SIZE]> =
    ConstStaticCell::new([0u8; TCP_RX_BUF_SIZE]);
static TCP_TX_BUF: ConstStaticCell<[u8; TCP_TX_BUF_SIZE]> =
    ConstStaticCell::new([0u8; TCP_TX_BUF_SIZE]);
static MQTT_RX_BUF: ConstStaticCell<[u8; MQTT_RX_BUF_SIZE]> =
    ConstStaticCell::new([0u8; MQTT_RX_BUF_SIZE]);
static MQTT_TX_BUF: ConstStaticCell<[u8; MQTT_TX_BUF_SIZE]> =
    ConstStaticCell::new([0u8; MQTT_TX_BUF_SIZE]);

static AUDIT_OUTBOX: Channel<CriticalSectionRawMutex, Audit, AUDIT_OUTBOX_CAPACITY> =
    Channel::new();
static TELEMETRY_OUTBOX: Channel<
    CriticalSectionRawMutex,
    heapless::String<MAX_TELEMETRY_LEN>,
    TELEMETRY_OUTBOX_CAPACITY,
> = Channel::new();

/// Returns `true` if the MQTT client currently has an active broker session.
pub fn is_connected() -> bool {
    MQTT_CONNECTED.load(Ordering::Relaxed)
}

/// Enqueue an [`Audit`] record for QoS 1 (`AtLeastOnce`) publication on
/// `doorsys/audit/<client_id>`.
///
/// Safe to call while offline; records are buffered in [`AUDIT_OUTBOX`] up to
/// [`AUDIT_OUTBOX_CAPACITY`] and flushed once the broker connection is established.
pub fn enqueue_audit(audit: Audit) {
    if AUDIT_OUTBOX.try_send(audit).is_err() {
        warn!("MQTT audit outbox full; dropping audit record");
    }
}

/// Enqueue a health telemetry line for QoS 0 (`AtMostOnce`) publication on `doorsys/status`.
///
/// Drops the line silently when [`is_connected`] is `false` so fire-and-forget telemetry only
/// flows over an active connection.
pub fn publish_telemetry(line: &str) {
    if !is_connected() {
        return;
    }
    let Ok(owned) = heapless::String::try_from(line) else {
        warn!(
            "Telemetry line too long ({} bytes, max {}); dropping",
            line.len(),
            MAX_TELEMETRY_LEN
        );
        return;
    };
    if TELEMETRY_OUTBOX.try_send(owned).is_err() {
        warn!("MQTT telemetry outbox full; dropping status line");
    }
}

/// Connection parameters for [`run`].
pub struct MqttSettings {
    pub host: heapless::String<64>,
    pub port: u16,
    pub client_id: heapless::String<32>,
    pub username: heapless::String<64>,
    pub password: heapless::String<64>,
}

/// Run the MQTT connection supervisor forever, invoking `on_message(topic, payload)` for every
/// incoming publication on `subscription_topic`.
pub async fn run<H>(
    stack: Stack<'static>,
    settings: MqttSettings,
    subscription_topic: &'static str,
    mut on_message: H,
) -> !
where
    H: AsyncFnMut(&str, &[u8]),
{
    // Statically allocated; `run` must therefore only be called once.
    let tcp_rx_buf = TCP_RX_BUF.take();
    let tcp_tx_buf = TCP_TX_BUF.take();
    let mqtt_rx_buf = MQTT_RX_BUF.take();
    let mqtt_tx_buf = MQTT_TX_BUF.take();

    let mut audit_topic = heapless::String::<MAX_AUDIT_TOPIC_LEN>::new();
    let _ = write!(audit_topic, "doorsys/audit/{}", settings.client_id.as_str());

    let mut config = ConfigBuilder::new(Buffers::new(mqtt_rx_buf, mqtt_tx_buf))
        .client_id(settings.client_id.as_str())
        .expect("client_id fits in 64 bytes")
        .keepalive_interval(KEEP_ALIVE_SECS)
        .session_expiry_interval(SESSION_EXPIRY_SECS)
        .autodowngrade_qos();

    if !settings.username.is_empty() || !settings.password.is_empty() {
        config = config
            .auth(settings.username.as_str(), settings.password.as_bytes())
            .expect("single auth configuration");
    }

    let mut session = Session::new(config);
    let audit_rx = AUDIT_OUTBOX.receiver();
    let telemetry_rx = TELEMETRY_OUTBOX.receiver();

    loop {
        MQTT_CONNECTED.store(false, Ordering::Relaxed);
        stack.wait_config_up().await;

        let remote_ip = match stack
            .dns_query(settings.host.as_str(), DnsQueryType::A)
            .await
        {
            Ok(addrs) if !addrs.is_empty() => addrs[0],
            Ok(_) => {
                warn!("MQTT DNS returned no addresses for {}", settings.host);
                Timer::after(RECONNECT_DELAY).await;
                continue;
            }
            Err(e) => {
                warn!("MQTT DNS lookup failed for {}: {:?}", settings.host, e);
                Timer::after(RECONNECT_DELAY).await;
                continue;
            }
        };

        let mut socket = TcpSocket::new(stack, &mut tcp_rx_buf[..], &mut tcp_tx_buf[..]);

        let endpoint = IpEndpoint::new(remote_ip, settings.port);
        info!("Connecting to MQTT broker at {}...", endpoint);
        if let Err(e) = socket.connect(endpoint).await {
            warn!("MQTT TCP connect failed: {:?}", e);
            Timer::after(RECONNECT_DELAY).await;
            continue;
        }

        let mut conn = match session.connect(socket).await {
            Ok(conn) => conn,
            Err(e) => {
                error!("MQTT handshake failed: {:?}", e);
                Timer::after(RECONNECT_DELAY).await;
                continue;
            }
        };

        match conn.connect_event() {
            ConnectEvent::Connected => {
                let sub_opts = SubscriptionOptions::default()
                    .maximum_qos(QoS::AtLeastOnce)
                    .retain_behavior(RetainHandling::IfSubscriptionDoesNotExist);
                let filter = TopicFilter::new(subscription_topic).options(sub_opts);
                if let Err(e) = conn.subscribe(&[filter], &[]).await {
                    error!("Failed to subscribe to {}: {:?}", subscription_topic, e);
                    Timer::after(RECONNECT_DELAY).await;
                    continue;
                }
                info!("MQTT connected (fresh session); subscribed to {}", subscription_topic);
            }
            ConnectEvent::Reconnected => {
                info!("MQTT connected (resumed existing broker session)");
            }
        }

        MQTT_CONNECTED.store(true, Ordering::Relaxed);

        'session: loop {
            // If in-flight QoS 1 capacity is full, wait for broker PUBACKs before dequeuing more
            // outbound messages so audits remain safely queued in AUDIT_OUTBOX.
            if !conn.can_publish(QoS::AtLeastOnce) {
                match conn.poll().await {
                    Ok(Some(msg)) => {
                        info!(
                            "MQTT message received on {}: {} bytes",
                            msg.topic(),
                            msg.payload().len()
                        );
                        on_message(msg.topic(), msg.payload()).await;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("MQTT poll error while awaiting PUBACK: {:?}", e);
                        break 'session;
                    }
                }
                continue 'session;
            }

            match select3(conn.recv(), audit_rx.receive(), telemetry_rx.receive()).await {
                Either3::First(Ok(msg)) => {
                    info!(
                        "MQTT message received on {}: {} bytes",
                        msg.topic(),
                        msg.payload().len()
                    );
                    on_message(msg.topic(), msg.payload()).await;
                }
                Either3::First(Err(e)) => {
                    warn!("MQTT recv error: {:?}", e);
                    break 'session;
                }
                Either3::Second(audit) => {
                    let mut buf = [0u8; 32];
                    let Ok(payload) = postcard::to_slice(&audit, &mut buf) else {
                        error!("Failed to serialize audit record");
                        continue 'session;
                    };
                    let pub_pkt = Publication::bytes(audit_topic.as_str(), payload)
                        .qos(QoS::AtLeastOnce);
                    if let Err(e) = conn.publish(pub_pkt).await {
                        warn!("MQTT audit publish error: {:?}", defmt::Debug2Format(&e));
                        // If the error occurred before minimq retained the packet in its TX arena,
                        // re-queue it into AUDIT_OUTBOX so the audit record is not lost.
                        if matches!(
                            e,
                            minimq::PubError::Session(
                                minimq::Error::Disconnected
                                    | minimq::Error::NotReady
                                    | minimq::Error::Resource(_)
                            )
                        ) {
                            let _ = AUDIT_OUTBOX.try_send(audit);
                        }
                        break 'session;
                    }
                }
                Either3::Third(line) => {
                    let pub_pkt =
                        Publication::bytes(STATUS_TOPIC, line.as_bytes()).qos(QoS::AtMostOnce);
                    if let Err(e) = conn.publish(pub_pkt).await {
                        warn!(
                            "MQTT telemetry publish error: {:?}",
                            defmt::Debug2Format(&e)
                        );
                        break 'session;
                    }
                }
            }
        }

        MQTT_CONNECTED.store(false, Ordering::Relaxed);
        Timer::after(RECONNECT_DELAY).await;
    }
}
