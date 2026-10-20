//! Reusable MQTT v5 client supervisor built on `rust-mqtt` and `embassy-net`.
//!
//! Manages DNS resolution, TCP connection, persistent MQTT v5 session recovery across network
//! drops and hard MCU reboots (`clean_start = false` + `SessionExpiryInterval`), conditional topic
//! subscription via `session_present`, cancel-safe packet reception (`poll_header`/`poll_body`),
//! zero-heap static packet buffering (`BumpBuffer`), QoS 1 `DUP` replay (`republish`), and separate
//! outgoing channels for [`Audit`] records and health telemetry.

use core::{
    fmt::Write as _,
    num::NonZero,
    sync::atomic::{AtomicBool, Ordering},
};

use defmt::{error, info, warn};
use doorsys_protocol::Audit;
use embassy_futures::select::{Either, Either4, select, select4};
use embassy_net::{IpEndpoint, Stack, dns::DnsQueryType, tcp::TcpSocket};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Duration, Instant, Timer};
use rust_mqtt::{
    Bytes,
    buffer::BumpBuffer,
    client::{
        Client, MqttError,
        event::{Event, Suback},
        options::{
            ConnectOptions, PublicationOptions, RetainHandling, SubscriptionOptions, TopicReference,
        },
    },
    config::{KeepAlive, SessionExpiryInterval},
    session::Session,
    types::{MqttBinary, MqttString, TopicFilter, TopicName},
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

/// MQTT keep-alive interval in seconds advertised in `CONNECT`.
pub const KEEP_ALIVE_SECS: u16 = 30;

/// Interval between `PINGREQ` packets when no outbound packets have been transmitted.
const PING_INTERVAL: Duration = Duration::from_secs(20);

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
    let mqtt_buf = MQTT_BUF.take();

    let mut audit_topic = heapless::String::<MAX_AUDIT_TOPIC_LEN>::new();
    let _ = write!(audit_topic, "doorsys/audit/{}", settings.client_id.as_str());

    let audit_rx = AUDIT_OUTBOX.receiver();
    let telemetry_rx = TELEMETRY_OUTBOX.receiver();

    let mut session = DoorsysSession::default();
    let mut pending_audit: Option<Audit> = None;
    // Assume true on boot so that if the broker reports `session_present == true` after a hard MCU
    // reset, we trust the broker's persisted subscription and skip re-subscribing. If the broker
    // reports `session_present == false`, we set `subscribed = false` until `SUBACK` succeeds.
    let mut subscribed = true;

    let session_expiry = if SESSION_EXPIRY_SECS == u32::MAX {
        SessionExpiryInterval::NeverEnd
    } else {
        SessionExpiryInterval::Seconds(
            NonZero::new(SESSION_EXPIRY_SECS).expect("SESSION_EXPIRY_SECS must be > 0"),
        )
    };

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

        let mut bump = BumpBuffer::new(&mut mqtt_buf[..]);
        let mut client: DoorsysMqttClient<'_, '_, TcpSocket<'_>> =
            Client::with_session(session.clone(), &mut bump);

        let Ok(client_id) = MqttString::try_from(settings.client_id.as_str()) else {
            error!("Invalid MQTT client_id");
            Timer::after(RECONNECT_DELAY).await;
            continue;
        };

        // NOTE: Do NOT call `.clean_start()` so Clean Start remains FALSE across reboots.
        let mut connect_opts = ConnectOptions::new()
            .keep_alive(KeepAlive::Seconds(
                NonZero::new(KEEP_ALIVE_SECS).expect("KEEP_ALIVE_SECS must be > 0"),
            ))
            .session_expiry_interval(session_expiry)
            .maximum_packet_size(
                NonZero::new(MQTT_BUF_SIZE as u32).expect("MQTT_BUF_SIZE must be > 0"),
            );

        if !settings.username.is_empty()
            && let Ok(u) = MqttString::try_from(settings.username.as_str())
        {
            connect_opts = connect_opts.user_name(u);
        }
        if !settings.password.is_empty()
            && let Ok(p) = MqttBinary::try_from(settings.password.as_bytes())
        {
            connect_opts = connect_opts.password(p);
        }

        let session_present = match client.connect(socket, &connect_opts, Some(client_id)).await {
            Ok(connected) => {
                let present = connected.session_present;
                info!("MQTT connected (session_present={})", present);
                present
            }
            Err(e) => {
                error!("MQTT handshake failed: {:?}", e);
                Timer::after(RECONNECT_DELAY).await;
                continue;
            }
        };
        // Safety: `connected` (or `e`) has been dropped; no references into `BumpBuffer` remain.
        unsafe { client.buffer_mut().reset() };

        // Subscribe only if the broker did not retain our session (or a prior cold subscribe did
        // not complete before disconnecting).
        if !session_present || !subscribed {
            subscribed = false;
            let Ok(topic_str) = MqttString::try_from(subscription_topic) else {
                error!("Invalid subscription topic {}", subscription_topic);
                Timer::after(RECONNECT_DELAY).await;
                continue;
            };
            let Some(filter) = TopicFilter::new(topic_str) else {
                error!("Invalid subscription topic filter {}", subscription_topic);
                Timer::after(RECONNECT_DELAY).await;
                continue;
            };
            let sub_opts = SubscriptionOptions::new()
                .retain_handling(RetainHandling::SendIfNotSubscribedBefore)
                .at_least_once();

            if let Err(e) = client.subscribe(filter, &sub_opts).await {
                error!(
                    "Failed to send SUBSCRIBE for {}: {:?}",
                    subscription_topic, e
                );
                session = client.session().clone();
                Timer::after(RECONNECT_DELAY).await;
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
        if let Some(ref audit) = pending_audit {
            let existing_pid = client
                .session()
                .outbound_publishes
                .first()
                .map(|(pid, _)| *pid);
            if let Err(e) =
                send_or_replay_audit(&mut client, audit_topic.as_str(), audit, existing_pid).await
            {
                warn!("MQTT audit publish error: {:?}", e);
                session = client.session().clone();
                Timer::after(RECONNECT_DELAY).await;
                continue;
            }
        }

        MQTT_CONNECTED.store(true, Ordering::Relaxed);
        let mut next_ping = Instant::now() + PING_INTERVAL;

        'session: loop {
            // While an outbound QoS 1 Audit is in flight, wait for incoming packets (including its
            // PUBACK or queued broker messages) and keep-alive pings before dequeuing another Audit.
            if pending_audit.is_some() {
                match select(client.poll_header(), Timer::at(next_ping)).await {
                    Either::First(Ok(header)) => {
                        let keep_running = match client.poll_body(header).await {
                            Ok(event) => {
                                handle_event(
                                    event,
                                    &mut pending_audit,
                                    &mut subscribed,
                                    subscription_topic,
                                    &mut on_message,
                                )
                                .await
                            }
                            Err(e) => {
                                warn!("MQTT poll_body error while awaiting PUBACK: {:?}", e);
                                false
                            }
                        };
                        // Safety: `event` (or `e`) has been dropped; no references into `BumpBuffer` remain.
                        unsafe { client.buffer_mut().reset() };
                        if !keep_running {
                            break 'session;
                        }
                    }
                    Either::First(Err(e)) => {
                        warn!("MQTT poll_header error while awaiting PUBACK: {:?}", e);
                        break 'session;
                    }
                    Either::Second(()) => {
                        if let Err(e) = client.ping().await {
                            warn!("MQTT ping failed: {:?}", e);
                            break 'session;
                        }
                        next_ping = Instant::now() + PING_INTERVAL;
                    }
                }
                continue 'session;
            }

            match select4(
                client.poll_header(),
                audit_rx.receive(),
                telemetry_rx.receive(),
                Timer::at(next_ping),
            )
            .await
            {
                Either4::First(Ok(header)) => {
                    let keep_running = match client.poll_body(header).await {
                        Ok(event) => {
                            handle_event(
                                event,
                                &mut pending_audit,
                                &mut subscribed,
                                subscription_topic,
                                &mut on_message,
                            )
                            .await
                        }
                        Err(e) => {
                            warn!("MQTT poll_body error: {:?}", e);
                            false
                        }
                    };
                    // Safety: `event` (or `e`) has been dropped; no references into `BumpBuffer` remain.
                    unsafe { client.buffer_mut().reset() };
                    if !keep_running {
                        break 'session;
                    }
                }
                Either4::First(Err(e)) => {
                    warn!("MQTT poll_header error: {:?}", e);
                    break 'session;
                }
                Either4::Second(audit) => {
                    let audit_ref = pending_audit.insert(audit);
                    if let Err(e) =
                        send_or_replay_audit(&mut client, audit_topic.as_str(), audit_ref, None)
                            .await
                    {
                        warn!("MQTT audit publish error: {:?}", e);
                        break 'session;
                    }
                    next_ping = Instant::now() + PING_INTERVAL;
                }
                Either4::Third(telemetry) => {
                    if let Err(e) =
                        send_telemetry(&mut client, settings.client_id.as_str(), &telemetry).await
                    {
                        warn!("MQTT telemetry publish error: {:?}", e);
                        break 'session;
                    }
                    next_ping = Instant::now() + PING_INTERVAL;
                }
                Either4::Fourth(()) => {
                    if let Err(e) = client.ping().await {
                        warn!("MQTT ping failed: {:?}", e);
                        break 'session;
                    }
                    next_ping = Instant::now() + PING_INTERVAL;
                }
            }
        }

        session = client.session().clone();
        MQTT_CONNECTED.store(false, Ordering::Relaxed);
        Timer::after(RECONNECT_DELAY).await;
    }
}

/// Format and publish a [`Telemetry`] snapshot as 3 QoS 0 InfluxDB line protocol messages
/// (`heap`, `storage`, and `status`) on [`STATUS_TOPIC`].
async fn send_telemetry<'c>(
    client: &mut DoorsysMqttClient<'_, 'c, TcpSocket<'_>>,
    host: &str,
    t: &Telemetry,
) -> Result<(), MqttError<'c, 0>> {
    let mut line = heapless::String::<MAX_TELEMETRY_LEN>::new();
    let version = t.version;
    let time = t.timestamp_nanos;

    // 1. Heap metrics
    let _ = write!(
        line,
        "heap,host={host} free={},used={},total={},max_used={} {time}",
        t.heap_free, t.heap_used, t.heap_total, t.heap_max_used
    );
    publish_status_line(client, line.as_bytes()).await?;

    // 2. Storage metrics
    line.clear();
    let _ = write!(
        line,
        "storage,host={host} capacity={} {time}",
        t.storage_capacity
    );
    publish_status_line(client, line.as_bytes()).await?;

    // 3. General status metrics (uptime and stored code count)
    line.clear();
    let _ = write!(
        line,
        "status,host={host} version={version},uptime={},codes={} {time}",
        t.uptime_secs, t.codes_count
    );
    publish_status_line(client, line.as_bytes()).await
}

async fn publish_status_line<'c>(
    client: &mut DoorsysMqttClient<'_, 'c, TcpSocket<'_>>,
    payload: &[u8],
) -> Result<(), MqttError<'c, 0>> {
    let topic_str = MqttString::try_from(STATUS_TOPIC).expect("valid STATUS_TOPIC");
    let topic_name = TopicName::new(topic_str).expect("valid STATUS_TOPIC");
    let pub_opts = PublicationOptions::new(TopicReference::Name(topic_name));
    client
        .publish(&pub_opts, Bytes::from(payload))
        .await
        .map(drop)
}

/// Publish a new QoS 1 [`Audit`] packet (`republish_pid == None`) or retransmit an unacknowledged
/// [`Audit`] packet with `DUP = 1` (`republish_pid == Some(pid)`).
async fn send_or_replay_audit<'c>(
    client: &mut DoorsysMqttClient<'_, 'c, TcpSocket<'_>>,
    audit_topic: &str,
    audit: &Audit,
    republish_pid: Option<rust_mqtt::types::PacketIdentifier>,
) -> Result<(), MqttError<'c, 0>> {
    let mut buf = [0u8; 32];
    let Ok(payload) = postcard::to_slice(audit, &mut buf) else {
        error!("Failed to serialize audit record");
        return Ok(());
    };
    let Ok(topic_str) = MqttString::try_from(audit_topic) else {
        error!("Invalid audit topic {}", audit_topic);
        return Ok(());
    };
    let Some(topic_name) = TopicName::new(topic_str) else {
        error!("Invalid audit topic name {}", audit_topic);
        return Ok(());
    };
    let pub_opts = PublicationOptions::new(TopicReference::Name(topic_name)).at_least_once();

    match republish_pid {
        Some(pid) => client.republish(pid, &pub_opts, Bytes::from(payload)).await,
        None => client
            .publish(&pub_opts, Bytes::from(payload))
            .await
            .map(drop),
    }
}

/// Handle a single decoded MQTT [`Event`], returning `true` if the session remains healthy.
async fn handle_event<H>(
    event: Event<'_, 0, 0>,
    pending_audit: &mut Option<Audit>,
    subscribed: &mut bool,
    subscription_topic: &str,
    on_message: &mut H,
) -> bool
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
            true
        }
        Event::PublishAcknowledged(ack) => {
            info!(
                "QoS 1 PUBACK received (id={}, reason={:?})",
                ack.packet_identifier.get(),
                ack.reason_code
            );
            *pending_audit = None;
            true
        }
        Event::PublishRejected(rej) => {
            warn!(
                "QoS 1 publish rejected (id={}, reason={:?})",
                rej.packet_identifier.get(),
                rej.reason_code
            );
            *pending_audit = None;
            true
        }
        Event::Suback(Suback { reason_code, .. }) => {
            if reason_code.is_success() {
                *subscribed = true;
                info!("Subscribed to {} ({:?})", subscription_topic, reason_code);
                true
            } else {
                error!(
                    "Subscription to {} rejected: {:?}",
                    subscription_topic, reason_code
                );
                false
            }
        }
        Event::Pingresp | Event::Ignored => true,
        other => {
            info!("MQTT event: {:?}", other);
            true
        }
    }
}
