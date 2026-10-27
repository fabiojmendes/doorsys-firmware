//! Reusable MQTT v5 client supervisor built on `rust-mqtt` and `embassy-net`.
//!
//! Manages DNS resolution, TCP connection, persistent MQTT v5 session recovery across network
//! drops and hard MCU reboots (`clean_start = false` + `SessionExpiryInterval`), conditional topic
//! subscription via `session_present`, cancel-safe packet reception (`poll_header`/`poll_body`),
//! zero-heap static packet buffering (`BumpBuffer`), QoS 1 `DUP` replay (`republish`), and separate
//! outgoing channels for [`Audit`] records and health telemetry.
//!
//! Reliability features:
//! - **Liveness detection:** a `PINGRESP` (or any other inbound packet) must arrive within half
//!   the negotiated keep-alive after each `PINGREQ`, and the TCP socket aborts when the peer stops
//!   ACKing, so half-open connections are detected instead of hanging forever.
//! - **Apply-then-acknowledge:** incoming QoS 1 publications are acknowledged manually *after*
//!   `on_message` returns, so a reset mid-handling causes the broker to redeliver the message.
//! - **Exponential reconnect backoff** with jitter.

use core::{
    fmt::Write as _,
    future::pending,
    num::NonZero,
    sync::atomic::{AtomicBool, Ordering},
};

use defmt::{error, info, warn};
use doorsys_protocol::Audit;
use embassy_futures::select::{Either4, select4};
use embassy_net::{IpEndpoint, Stack, dns::DnsQueryType, tcp::TcpSocket};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Duration, Instant, Timer};
use rust_mqtt::{
    Bytes,
    buffer::BumpBuffer,
    client::{
        Client, MqttError,
        event::{Event, Publish, Suback},
        options::{
            AckMode, AckOptions, ConnectOptions, PublicationOptions, RetainHandling,
            SubscriptionOptions, TopicReference,
        },
    },
    config::{KeepAlive, SessionExpiryInterval},
    session::Session,
    types::{
        IdentifiedQoS, MqttBinary, MqttString, PacketIdentifier, ReasonCode, TopicFilter, TopicName,
    },
};
use static_cell::ConstStaticCell;

use crate::health::Telemetry;

// ============================================================================
// Tuning & Buffer Configuration Constants
// ============================================================================

/// Maximum number of [`Audit`] records queued in RAM while the device is offline or backpressured.
///
/// Each slot stores a 24-byte [`Audit`] struct in `.bss` (`128 * 24 = 3,072` bytes).
/// Increase this value (e.g. `256` = 6 KiB, `512` = 12 KiB) to retain more door access events
/// across extended Wi-Fi or broker outages.
pub const AUDIT_OUTBOX_CAPACITY: usize = 128;

/// Maximum number of [`Telemetry`] snapshots queued in RAM while connected.
///
/// [`crate::health`] emits one [`Telemetry`] struct (~56 bytes) every 60 seconds, which the MQTT
/// task expands into 3 InfluxDB line protocol messages (`heap`, `storage`, and `status`).
pub const TELEMETRY_OUTBOX_CAPACITY: usize = 1;

/// Maximum byte length of a single formatted health telemetry line on `doorsys/status`.
pub const MAX_TELEMETRY_LEN: usize = 256;

/// Size in bytes of the static [`BumpBuffer`] arena used by `rust-mqtt` for incoming packets,
/// and advertised to the broker via the MQTT v5 `MaximumPacketSize` property.
///
/// `rust-mqtt` streams outgoing packets directly to the TCP socket without allocating from
/// [`BumpBuffer`], and we reset the bump pointer after every received packet, so this buffer only
/// ever holds one incoming packet at a time. Size this for the largest `UserAction::Bulk(Vec<i32>)`
/// payload expected (~4–5 bytes per user code in `postcard`; 4096 bytes fits ~800–1,000 codes).
pub const MQTT_BUF_SIZE: usize = 4096;

/// Size in bytes of the underlying `embassy-net` TCP socket receive ring buffer.
pub const TCP_RX_BUF_SIZE: usize = 4096;

/// Size in bytes of the underlying `embassy-net` TCP socket transmit ring buffer.
pub const TCP_TX_BUF_SIZE: usize = 2048;

/// MQTT v5 `ReceiveMaximum` advertised in `CONNECT`: maximum number of unacknowledged inbound
/// QoS 1/2 publications the broker is permitted to have in flight concurrently.
pub const RECEIVE_MAXIMUM: usize = 4;

/// Maximum number of unacknowledged outbound QoS 1 [`Audit`] publications in flight at once.
///
/// Keeping `SEND_MAXIMUM = 1` serializes [`Audit`] delivery in strict FIFO order while keeping
/// remaining records queued in [`AUDIT_OUTBOX`].
pub const SEND_MAXIMUM: usize = 1;

/// Broker-side MQTT v5 `SessionExpiryInterval` in seconds.
///
/// `86_400` instructs the broker to retain our subscriptions and queue offline QoS 1 messages for
/// 24 hours across network drops or MCU reboots. Set to `u32::MAX` for
/// [`SessionExpiryInterval::NeverEnd`].
pub const SESSION_EXPIRY_SECS: u32 = 86_400;

/// MQTT keep-alive interval in seconds requested in `CONNECT`.
///
/// The broker may override this via `ServerKeepAlive`; ping and liveness timing always follow the
/// negotiated value.
pub const KEEP_ALIVE_SECS: u16 = 30;

/// Initial delay between reconnect attempts when DNS, TCP, or MQTT handshakes fail.
const RECONNECT_DELAY_MIN: Duration = Duration::from_secs(1);

/// Upper bound for the exponential reconnect backoff.
const RECONNECT_DELAY_MAX: Duration = Duration::from_secs(60);

/// TCP retransmission timeout: the socket is aborted if the broker stops ACKing sent data for this
/// long. Backstop for writes blocked on a full TX buffer towards a dead peer.
const TCP_TIMEOUT: Duration = Duration::from_secs(KEEP_ALIVE_SECS as u64 * 2);

/// Maximum byte length of a formatted `doorsys/audit/<client_id>` topic string.
const MAX_AUDIT_TOPIC_LEN: usize = 64;

/// Maximum `postcard` encoded size of an [`Audit`] record (worst case is ~17 bytes).
const MAX_AUDIT_PAYLOAD_LEN: usize = 32;

/// Topic used for periodic health telemetry publications.
const STATUS_TOPIC: &str = "doorsys/status";

const KEEP_ALIVE: NonZero<u16> =
    NonZero::new(KEEP_ALIVE_SECS).expect("KEEP_ALIVE_SECS must be > 0");

const MAX_PACKET_SIZE: NonZero<u32> =
    NonZero::new(MQTT_BUF_SIZE as u32).expect("MQTT_BUF_SIZE must be > 0");

const SESSION_EXPIRY: SessionExpiryInterval = if SESSION_EXPIRY_SECS == u32::MAX {
    SessionExpiryInterval::NeverEnd
} else {
    SessionExpiryInterval::Seconds(
        NonZero::new(SESSION_EXPIRY_SECS).expect("SESSION_EXPIRY_SECS must be > 0"),
    )
};

// ============================================================================
// Static Buffers & Channels
// ============================================================================

static MQTT_CONNECTED: AtomicBool = AtomicBool::new(false);

static TCP_RX_BUF: ConstStaticCell<[u8; TCP_RX_BUF_SIZE]> =
    ConstStaticCell::new([0u8; TCP_RX_BUF_SIZE]);
static TCP_TX_BUF: ConstStaticCell<[u8; TCP_TX_BUF_SIZE]> =
    ConstStaticCell::new([0u8; TCP_TX_BUF_SIZE]);
static MQTT_BUF: ConstStaticCell<[u8; MQTT_BUF_SIZE]> = ConstStaticCell::new([0u8; MQTT_BUF_SIZE]);

static AUDIT_OUTBOX: Channel<CriticalSectionRawMutex, Audit, AUDIT_OUTBOX_CAPACITY> =
    Channel::new();
static TELEMETRY_OUTBOX: Channel<CriticalSectionRawMutex, Telemetry, TELEMETRY_OUTBOX_CAPACITY> =
    Channel::new();

type DoorsysSession = Session<1, RECEIVE_MAXIMUM, SEND_MAXIMUM, 0, 0>;
type DoorsysMqttClient<'a, 'c, T> =
    Client<'a, 'c, T, BumpBuffer<'c>, 1, RECEIVE_MAXIMUM, SEND_MAXIMUM, 0, 0, 0, 0>;

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

/// Enqueue a [`Telemetry`] snapshot for QoS 0 (`AtMostOnce`) publication on `doorsys/status`.
///
/// Drops the snapshot silently when [`is_connected`] is `false` so fire-and-forget telemetry only
/// flows over an active connection.
pub fn publish_telemetry(telemetry: Telemetry) {
    if !is_connected() {
        return;
    }
    if TELEMETRY_OUTBOX.try_send(telemetry).is_err() {
        warn!("MQTT telemetry outbox full; dropping telemetry snapshot");
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

/// An [`Audit`] record that has been dequeued and encoded, awaiting its `PUBACK`.
struct InFlightAudit {
    payload: [u8; MAX_AUDIT_PAYLOAD_LEN],
    len: usize,
    /// Packet identifier assigned by `rust-mqtt` once the `PUBLISH` was sent.
    pid: Option<PacketIdentifier>,
}

impl InFlightAudit {
    /// Encode `audit`, returning `None` (and logging) if it does not fit the payload buffer.
    fn encode(audit: &Audit) -> Option<Self> {
        let mut payload = [0u8; MAX_AUDIT_PAYLOAD_LEN];
        match postcard::to_slice(audit, &mut payload) {
            Ok(encoded) => {
                let len = encoded.len();
                Some(Self {
                    payload,
                    len,
                    pid: None,
                })
            }
            Err(e) => {
                error!("Failed to serialize audit record; dropping it: {:?}", e);
                None
            }
        }
    }

    fn payload(&self) -> &[u8] {
        &self.payload[..self.len]
    }
}

/// Exponential reconnect backoff with up to 25% jitter.
struct Backoff {
    current: Duration,
}

impl Backoff {
    const fn new() -> Self {
        Self {
            current: RECONNECT_DELAY_MIN,
        }
    }

    fn reset(&mut self) {
        self.current = RECONNECT_DELAY_MIN;
    }

    async fn wait(&mut self) {
        let max_jitter_ms = self.current.as_millis() / 4 + 1;
        let jitter = Duration::from_millis(Instant::now().as_ticks() % max_jitter_ms);
        let delay = self.current + jitter;
        info!("MQTT reconnecting in {} ms", delay.as_millis());
        Timer::after(delay).await;
        self.current = (self.current * 2).min(RECONNECT_DELAY_MAX);
    }
}

/// Log a permanent configuration error and park the task forever (retrying cannot fix it).
async fn park() -> ! {
    error!("MQTT client disabled due to invalid configuration");
    loop {
        pending::<()>().await;
    }
}

/// Run the MQTT connection supervisor forever, invoking `on_message(topic, payload)` for every
/// incoming publication on `subscription_topic`.
///
/// Incoming QoS 1 publications are acknowledged after `on_message` returns, regardless of whether
/// the handler succeeded, so a malformed or unprocessable message never causes a redelivery loop.
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
    let mqtt_buf = MQTT_BUF.take();

    // ------------------------------------------------------------------------
    // Validate static configuration once; these errors are permanent.
    // ------------------------------------------------------------------------
    let mut audit_topic = heapless::String::<MAX_AUDIT_TOPIC_LEN>::new();
    if write!(audit_topic, "doorsys/audit/{}", settings.client_id.as_str()).is_err() {
        error!(
            "MQTT audit topic too long for client_id {}",
            settings.client_id
        );
        park().await;
    }
    let Some(audit_topic_name) = MqttString::try_from(audit_topic.as_str())
        .ok()
        .and_then(TopicName::new)
    else {
        error!("Invalid MQTT audit topic {}", audit_topic);
        park().await;
    };
    let Some(status_topic_name) = MqttString::try_from(STATUS_TOPIC)
        .ok()
        .and_then(TopicName::new)
    else {
        error!("Invalid MQTT status topic {}", STATUS_TOPIC);
        park().await;
    };
    let Some(subscription_filter) = MqttString::try_from(subscription_topic)
        .ok()
        .and_then(TopicFilter::new)
    else {
        error!("Invalid MQTT subscription topic {}", subscription_topic);
        park().await;
    };
    let Ok(client_id) = MqttString::try_from(settings.client_id.as_str()) else {
        error!("Invalid MQTT client_id {}", settings.client_id);
        park().await;
    };

    let audit_opts =
        PublicationOptions::new(TopicReference::Name(audit_topic_name)).at_least_once();
    let status_opts = PublicationOptions::new(TopicReference::Name(status_topic_name));
    let sub_opts = SubscriptionOptions::new()
        .retain_handling(RetainHandling::SendIfNotSubscribedBefore)
        .at_least_once();

    // NOTE: Do NOT call `.clean_start()` so Clean Start remains FALSE across reboots.
    let mut connect_opts = ConnectOptions::new()
        .keep_alive(KeepAlive::Seconds(KEEP_ALIVE))
        .session_expiry_interval(SESSION_EXPIRY)
        .maximum_packet_size(MAX_PACKET_SIZE);
    if !settings.username.is_empty() {
        match MqttString::try_from(settings.username.as_str()) {
            Ok(u) => connect_opts = connect_opts.user_name(u),
            Err(_) => warn!("Invalid MQTT username; connecting without it"),
        }
    }
    if !settings.password.is_empty() {
        match MqttBinary::try_from(settings.password.as_bytes()) {
            Ok(p) => connect_opts = connect_opts.password(p),
            Err(_) => warn!("Invalid MQTT password; connecting without it"),
        }
    }

    let audit_rx = AUDIT_OUTBOX.receiver();
    let telemetry_rx = TELEMETRY_OUTBOX.receiver();

    let mut session = DoorsysSession::default();
    let mut in_flight: Option<InFlightAudit> = None;
    let mut backoff = Backoff::new();
    // Assume true on boot so that if the broker reports `session_present == true` after a hard MCU
    // reset, we trust the broker's persisted subscription and skip re-subscribing. If the broker
    // reports `session_present == false`, we set `subscribed = false` until `SUBACK` succeeds.
    let mut subscribed = true;

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
                backoff.wait().await;
                continue;
            }
            Err(e) => {
                warn!("MQTT DNS lookup failed for {}: {:?}", settings.host, e);
                backoff.wait().await;
                continue;
            }
        };

        let mut socket = TcpSocket::new(stack, &mut tcp_rx_buf[..], &mut tcp_tx_buf[..]);
        socket.set_timeout(Some(TCP_TIMEOUT));

        let endpoint = IpEndpoint::new(remote_ip, settings.port);
        info!("Connecting to MQTT broker at {}...", endpoint);
        if let Err(e) = socket.connect(endpoint).await {
            warn!("MQTT TCP connect failed: {:?}", e);
            backoff.wait().await;
            continue;
        }

        let mut bump = BumpBuffer::new(&mut mqtt_buf[..]);
        let mut client: DoorsysMqttClient<'_, '_, TcpSocket<'_>> =
            Client::with_session(session.clone(), &mut bump);
        // Acknowledge incoming QoS 1 publications only after `on_message` has processed them.
        client.ack_manually_when(&ack_qos1_manually);

        let session_present = match client
            .connect(socket, &connect_opts, Some(client_id.clone()))
            .await
        {
            Ok(connected) => {
                let present = connected.session_present;
                info!("MQTT connected (session_present={})", present);
                present
            }
            Err(e) => {
                error!("MQTT handshake failed: {:?}", e);
                backoff.wait().await;
                continue;
            }
        };
        // Safety: `connected` (or `e`) has been dropped; no references into `BumpBuffer` remain.
        unsafe { client.buffer_mut().reset() };
        let connected_at = Instant::now();

        // Subscribe only if the broker did not retain our session (or a prior cold subscribe did
        // not complete before disconnecting).
        if !session_present || !subscribed {
            subscribed = false;
            if let Err(e) = client
                .subscribe(subscription_filter.clone(), &sub_opts)
                .await
            {
                error!(
                    "Failed to send SUBSCRIBE for {}: {:?}",
                    subscription_topic, e
                );
                session = client.session().clone();
                backoff.wait().await;
                continue;
            }
        } else {
            info!(
                "Resumed existing broker session; skipping SUBSCRIBE for {}",
                subscription_topic
            );
        }

        // If an Audit record was in flight when the previous connection dropped, replay it with
        // DUP=1 (`republish`) if the session was preserved, or send it fresh (`publish`) if not.
        if let Some(ref mut audit) = in_flight {
            let existing_pid = client
                .session()
                .outbound_publishes
                .first()
                .map(|(pid, _)| *pid);
            if let Err(e) = send_audit(&mut client, &audit_opts, audit, existing_pid).await {
                warn!("MQTT audit publish error: {:?}", e);
                session = client.session().clone();
                backoff.wait().await;
                continue;
            }
        }

        MQTT_CONNECTED.store(true, Ordering::Relaxed);

        // Ping and liveness timing follow the keep-alive negotiated with the broker.
        let keep_alive = match client.shared_config().keep_alive {
            KeepAlive::Seconds(s) => Duration::from_secs(s.get().into()),
            KeepAlive::Infinite => Duration::from_secs(KEEP_ALIVE_SECS.into()),
        };
        let ping_interval = keep_alive * 2 / 3;
        let pingresp_timeout = keep_alive / 2;
        let mut next_ping = Instant::now() + ping_interval;
        // Armed when a PINGREQ is sent; cleared by any inbound packet.
        let mut liveness_deadline: Option<Instant> = None;

        'session: loop {
            let wake_at = liveness_deadline.map_or(next_ping, |d| d.min(next_ping));
            // While an outbound QoS 1 Audit is in flight, do not dequeue another one; keep
            // processing incoming packets (including its PUBACK), telemetry and keep-alive pings.
            // `Channel::receive` is cancel-safe, so dropping this future loses nothing.
            let audit_gated = in_flight.is_some();
            let next_audit = async {
                if audit_gated {
                    pending::<Audit>().await
                } else {
                    audit_rx.receive().await
                }
            };

            match select4(
                client.poll_header(),
                next_audit,
                telemetry_rx.receive(),
                Timer::at(wake_at),
            )
            .await
            {
                Either4::First(Ok(header)) => {
                    liveness_deadline = None;
                    let keep_running = match client.poll_body(header).await {
                        Ok(event) => {
                            handle_event(
                                event,
                                &mut in_flight,
                                &mut subscribed,
                                subscription_topic,
                                &mut on_message,
                            )
                            .await
                        }
                        Err(e) => {
                            warn!("MQTT poll_body error: {:?}", e);
                            EventOutcome::Disconnect
                        }
                    };
                    // Safety: `event` (or `e`) has been dropped; no references into `BumpBuffer` remain.
                    unsafe { client.buffer_mut().reset() };
                    match keep_running {
                        EventOutcome::Continue => {}
                        EventOutcome::Acknowledge(pid) => {
                            if let Err(e) = client
                                .manual_acknowledge(pid, ReasonCode::Success, &AckOptions::new())
                                .await
                            {
                                warn!("MQTT PUBACK failed (id={}): {:?}", pid.get(), e);
                                break 'session;
                            }
                        }
                        EventOutcome::Disconnect => break 'session,
                    }
                }
                Either4::First(Err(e)) => {
                    warn!("MQTT poll_header error: {:?}", e);
                    break 'session;
                }
                Either4::Second(audit) => {
                    // Encoding failures are logged and the record skipped, so the in-flight slot
                    // can never get stuck holding an unsendable audit.
                    in_flight = InFlightAudit::encode(&audit);
                    let Some(audit) = in_flight.as_mut() else {
                        continue 'session;
                    };
                    if let Err(e) = send_audit(&mut client, &audit_opts, audit, None).await {
                        warn!("MQTT audit publish error: {:?}", e);
                        break 'session;
                    }
                    next_ping = Instant::now() + ping_interval;
                }
                Either4::Third(telemetry) => {
                    if let Err(e) =
                        send_telemetry(&mut client, &status_opts, &settings.client_id, &telemetry)
                            .await
                    {
                        warn!("MQTT telemetry publish error: {:?}", e);
                        break 'session;
                    }
                    next_ping = Instant::now() + ping_interval;
                }
                Either4::Fourth(()) => {
                    let now = Instant::now();
                    if liveness_deadline.is_some_and(|d| now >= d) {
                        warn!(
                            "MQTT broker unresponsive (no PINGRESP within {} s)",
                            pingresp_timeout.as_secs()
                        );
                        break 'session;
                    }
                    if now >= next_ping {
                        if let Err(e) = client.ping().await {
                            warn!("MQTT ping failed: {:?}", e);
                            break 'session;
                        }
                        liveness_deadline.get_or_insert(now + pingresp_timeout);
                        next_ping = now + ping_interval;
                    }
                }
            }
        }

        session = client.session().clone();
        MQTT_CONNECTED.store(false, Ordering::Relaxed);
        // Only reset the backoff after a session that proved stable, so a broker that accepts and
        // immediately drops us (e.g. auth/ACL churn, client-id takeover) is not hammered.
        if connected_at.elapsed() >= keep_alive {
            backoff.reset();
        }
        backoff.wait().await;
    }
}

/// `rust-mqtt` predicate selecting manual acknowledgement for incoming QoS 1 publications.
fn ack_qos1_manually(publish: &Publish<'_, 0, 0>) -> bool {
    matches!(publish.identified_qos, IdentifiedQoS::AtLeastOnce(_))
}

/// Format and publish a [`Telemetry`] snapshot as 3 QoS 0 InfluxDB line protocol messages
/// (`heap`, `storage`, and `status`) on [`STATUS_TOPIC`].
async fn send_telemetry<'c>(
    client: &mut DoorsysMqttClient<'_, 'c, TcpSocket<'_>>,
    status_opts: &PublicationOptions<'_>,
    client_id: &str,
    t: &Telemetry,
) -> Result<(), MqttError<'c, 0>> {
    let mut line = heapless::String::<MAX_TELEMETRY_LEN>::new();
    let version = t.version;
    let time = t.timestamp_nanos;

    // 1. Heap metrics
    let _ = write!(
        line,
        "heap,host={client_id} free={},used={},total={},max_used={} {time}",
        t.heap_free, t.heap_used, t.heap_total, t.heap_max_used
    );
    client
        .publish(status_opts, Bytes::from(line.as_bytes()))
        .await?;

    // 2. Storage metrics
    line.clear();
    let _ = write!(
        line,
        "storage,host={client_id} used={},capacity={} {time}",
        t.storage_used, t.storage_capacity
    );
    client
        .publish(status_opts, Bytes::from(line.as_bytes()))
        .await?;

    // 3. General status metrics (uptime, stored code count, and chip temperature)
    line.clear();
    let _ = write!(
        line,
        "doorsys,host={client_id},version={version} uptime={},codes={},temp={:.2} {time}",
        t.uptime_secs, t.codes_count, t.temperature_celsius
    );
    client
        .publish(status_opts, Bytes::from(line.as_bytes()))
        .await
        .map(drop)
}

/// Publish a new QoS 1 [`Audit`] packet (`republish_pid == None`) or retransmit an unacknowledged
/// [`Audit`] packet with `DUP = 1` (`republish_pid == Some(pid)`), recording the packet identifier
/// used so the matching `PUBACK` can be recognized.
async fn send_audit<'c>(
    client: &mut DoorsysMqttClient<'_, 'c, TcpSocket<'_>>,
    audit_opts: &PublicationOptions<'_>,
    audit: &mut InFlightAudit,
    republish_pid: Option<PacketIdentifier>,
) -> Result<(), MqttError<'c, 0>> {
    audit.pid = match republish_pid {
        Some(pid) => {
            client
                .republish(pid, audit_opts, Bytes::from(audit.payload()))
                .await?;
            Some(pid)
        }
        None => {
            client
                .publish(audit_opts, Bytes::from(audit.payload()))
                .await?
        }
    };
    Ok(())
}

/// Result of handling a single decoded MQTT [`Event`].
enum EventOutcome {
    /// The session remains healthy; nothing else to do.
    Continue,
    /// An incoming QoS 1 publication was processed and must now be acknowledged.
    Acknowledge(PacketIdentifier),
    /// The session must be torn down and re-established.
    Disconnect,
}

/// Clear the in-flight audit if `pid` matches its packet identifier.
fn complete_in_flight(in_flight: &mut Option<InFlightAudit>, pid: PacketIdentifier) {
    match in_flight {
        Some(audit) if audit.pid.is_none_or(|p| p == pid) => *in_flight = None,
        _ => warn!(
            "MQTT acknowledgement for unexpected packet id {}",
            pid.get()
        ),
    }
}

/// Handle a single decoded MQTT [`Event`].
async fn handle_event<H>(
    event: Event<'_, 0, 0>,
    in_flight: &mut Option<InFlightAudit>,
    subscribed: &mut bool,
    subscription_topic: &str,
    on_message: &mut H,
) -> EventOutcome
where
    H: AsyncFnMut(&str, &[u8]),
{
    match event {
        Event::Publish(pub_pkt) | Event::Duplicate(pub_pkt) => {
            if let Some(topic_name) = pub_pkt.topic.name() {
                let topic_str: &str = topic_name.as_ref().as_ref();
                let payload: &[u8] = &pub_pkt.message;
                info!(
                    "MQTT message received on {}: {} bytes (dup={})",
                    topic_str,
                    payload.len(),
                    pub_pkt.dup
                );
                on_message(topic_str, payload).await;
            }
            // Always acknowledge once handled (even on handler failure) to avoid redelivery loops.
            match (pub_pkt.ack_mode, pub_pkt.identified_qos) {
                (AckMode::Manual, IdentifiedQoS::AtLeastOnce(pid)) => {
                    EventOutcome::Acknowledge(pid)
                }
                _ => EventOutcome::Continue,
            }
        }
        Event::PublishAcknowledged(ack) => {
            info!(
                "QoS 1 PUBACK received (id={}, reason={:?})",
                ack.packet_identifier.get(),
                ack.reason_code
            );
            complete_in_flight(in_flight, ack.packet_identifier);
            EventOutcome::Continue
        }
        Event::PublishRejected(rej) => {
            warn!(
                "QoS 1 publish rejected; dropping audit (id={}, reason={:?})",
                rej.packet_identifier.get(),
                rej.reason_code
            );
            complete_in_flight(in_flight, rej.packet_identifier);
            EventOutcome::Continue
        }
        Event::Suback(Suback { reason_code, .. }) => {
            if reason_code.is_success() {
                *subscribed = true;
                info!("Subscribed to {} ({:?})", subscription_topic, reason_code);
                EventOutcome::Continue
            } else {
                error!(
                    "Subscription to {} rejected: {:?}",
                    subscription_topic, reason_code
                );
                EventOutcome::Disconnect
            }
        }
        Event::Pingresp | Event::Ignored => EventOutcome::Continue,
        other => {
            info!("MQTT event: {:?}", other);
            EventOutcome::Continue
        }
    }
}
