//! Reusable MQTT v5 client supervisor built on `rust-mqtt` and `embassy-net`.
//!
//! Manages DNS resolution, TCP connection, persistent MQTT v5 session recovery
//! (`clean_start = false`, `SessionExpiryInterval::NeverEnd`), topic subscription, keep-alive
//! pings, and draining an outgoing publication channel.

use alloc::{string::String, vec, vec::Vec};
use core::{
    num::NonZero,
    sync::atomic::{AtomicBool, Ordering},
};

use defmt::{error, info, warn};
use embassy_futures::select::{Either3, select3};
use embassy_net::{IpEndpoint, Stack, dns::DnsQueryType, tcp::TcpSocket};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    channel::{Channel, Receiver, Sender},
};
use embassy_time::{Duration, Timer};
use rust_mqtt::{
    Bytes,
    buffer::AllocBuffer,
    client::{
        Client,
        event::{Event, Suback},
        options::{
            ConnectOptions, PublicationOptions, RetainHandling, SubscriptionOptions, TopicReference,
        },
    },
    config::{KeepAlive, SessionExpiryInterval},
    types::{MqttBinary, MqttString, TopicFilter, TopicName},
};

const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const KEEP_ALIVE_SECS: u16 = 30;
const PING_INTERVAL: Duration = Duration::from_secs(20);
const TCP_RX_BUF_SIZE: usize = 4096;
const TCP_TX_BUF_SIZE: usize = 2048;
const MAX_PACKET_SIZE: u32 = 16 * 1024;
const OUTBOX_CAPACITY: usize = 32;

static MQTT_CONNECTED: AtomicBool = AtomicBool::new(false);

/// Returns `true` if the MQTT client currently has an active broker session.
pub fn is_connected() -> bool {
    MQTT_CONNECTED.load(Ordering::Relaxed)
}

/// Quality of Service level for outgoing messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, defmt::Format)]
pub enum PublishQoS {
    AtMostOnce,
    AtLeastOnce,
}

/// An outgoing MQTT publication queued for transmission.
pub struct OutgoingMessage {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: PublishQoS,
}

static OUTBOX: Channel<CriticalSectionRawMutex, OutgoingMessage, OUTBOX_CAPACITY> = Channel::new();

/// Obtain a sender handle to enqueue outgoing MQTT messages.
pub fn outbox_sender() -> Sender<'static, CriticalSectionRawMutex, OutgoingMessage, OUTBOX_CAPACITY>
{
    OUTBOX.sender()
}

/// Obtain a receiver handle for the MQTT supervisor task.
pub fn outbox_receiver()
-> Receiver<'static, CriticalSectionRawMutex, OutgoingMessage, OUTBOX_CAPACITY> {
    OUTBOX.receiver()
}

/// Try to enqueue a message without blocking; drops the message with a warning if the outbox is full.
pub fn try_enqueue(topic: impl Into<String>, payload: Vec<u8>, qos: PublishQoS) {
    if OUTBOX
        .try_send(OutgoingMessage {
            topic: topic.into(),
            payload,
            qos,
        })
        .is_err()
    {
        warn!("MQTT outbox full; dropping outgoing message");
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

type DoorsysMqttClient<'a, 'c, T> = Client<'a, 'c, T, AllocBuffer, 1, 4, 1, 0, 0, 0, 0>;

/// Run the MQTT connection supervisor forever, invoking `on_message(topic, payload)` for every
/// incoming publication on `subscription_topic`.
pub async fn run<H>(
    stack: Stack<'static>,
    settings: MqttSettings,
    subscription_topic: &'static str,
    outbox: Receiver<'static, CriticalSectionRawMutex, OutgoingMessage, OUTBOX_CAPACITY>,
    mut on_message: H,
) -> !
where
    H: AsyncFnMut(&str, &[u8]),
{
    let mut rx_buf = vec![0u8; TCP_RX_BUF_SIZE];
    let mut tx_buf = vec![0u8; TCP_TX_BUF_SIZE];

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

        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_timeout(Some(Duration::from_secs(15)));

        let endpoint = IpEndpoint::new(remote_ip, settings.port);
        info!("Connecting to MQTT broker at {}...", endpoint);
        if let Err(e) = socket.connect(endpoint).await {
            warn!("MQTT TCP connect failed: {:?}", e);
            Timer::after(RECONNECT_DELAY).await;
            continue;
        }

        let mut alloc_buffer = AllocBuffer;
        let mut client: DoorsysMqttClient<'_, '_, TcpSocket<'_>> = Client::new(&mut alloc_buffer);

        let Ok(client_id) = MqttString::try_from(settings.client_id.as_str()) else {
            error!("Invalid MQTT client_id");
            Timer::after(RECONNECT_DELAY).await;
            continue;
        };

        let mut connect_opts = ConnectOptions::new()
            .keep_alive(KeepAlive::Seconds(NonZero::new(KEEP_ALIVE_SECS).unwrap()))
            .session_expiry_interval(SessionExpiryInterval::NeverEnd)
            .maximum_packet_size(NonZero::new(MAX_PACKET_SIZE).unwrap());

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
                info!(
                    "MQTT connected (session_present={})",
                    connected.session_present
                );
                connected.session_present
            }
            Err(e) => {
                error!("MQTT handshake failed: {:?}", e);
                Timer::after(RECONNECT_DELAY).await;
                continue;
            }
        };

        // Always ensure we are subscribed to `subscription_topic`.
        if !session_present {
            if let Err(()) = subscribe_topic(&mut client, subscription_topic).await {
                Timer::after(RECONNECT_DELAY).await;
                continue;
            }
        } else {
            // Even when resuming a session, subscribing is idempotent with SendIfNotSubscribedBefore.
            let _ = subscribe_topic(&mut client, subscription_topic).await;
        }

        MQTT_CONNECTED.store(true, Ordering::Relaxed);

        let mut pending_qos1: Option<u16> = None;

        'session: loop {
            // While a QoS 1 publish is in flight, wait exclusively for incoming packets until
            // its PublishAcknowledged arrives so we never exceed SEND_MAXIMUM = 1.
            if pending_qos1.is_some() {
                match client.poll().await {
                    Ok(event) => {
                        if handle_event(event, &mut pending_qos1, &mut on_message).await {
                            continue 'session;
                        }
                    }
                    Err(e) => {
                        warn!("MQTT poll error while awaiting PUBACK: {:?}", e);
                        break 'session;
                    }
                }
                continue 'session;
            }

            match select3(
                client.poll_header(),
                outbox.receive(),
                Timer::after(PING_INTERVAL),
            )
            .await
            {
                Either3::First(Ok(header)) => match client.poll_body(header).await {
                    Ok(event) => {
                        let _ = handle_event(event, &mut pending_qos1, &mut on_message).await;
                    }
                    Err(e) => {
                        warn!("MQTT poll_body error: {:?}", e);
                        break 'session;
                    }
                },
                Either3::First(Err(e)) => {
                    warn!("MQTT poll_header error: {:?}", e);
                    break 'session;
                }
                Either3::Second(msg) => {
                    let Ok(topic_str) = MqttString::try_from(msg.topic.as_str()) else {
                        warn!("Invalid outgoing MQTT topic");
                        continue 'session;
                    };
                    let Some(topic_name) = TopicName::new(topic_str) else {
                        warn!("Invalid outgoing MQTT topic name");
                        continue 'session;
                    };
                    let topic_ref = TopicReference::Name(topic_name);
                    let pub_opts = match msg.qos {
                        PublishQoS::AtMostOnce => PublicationOptions::new(topic_ref),
                        PublishQoS::AtLeastOnce => {
                            PublicationOptions::new(topic_ref).at_least_once()
                        }
                    };
                    match client
                        .publish(&pub_opts, Bytes::from(msg.payload.as_slice()))
                        .await
                    {
                        Ok(pkt_id) => {
                            if let Some(id) = pkt_id {
                                pending_qos1 = Some(id.get().get());
                            }
                        }
                        Err(e) => {
                            warn!("MQTT publish error: {:?}", e);
                            // Re-queue QoS 1 messages (e.g. audits) so they are not lost on a broken socket.
                            if msg.qos == PublishQoS::AtLeastOnce {
                                let _ = OUTBOX.try_send(msg);
                            }
                            break 'session;
                        }
                    }
                }
                Either3::Third(()) => {
                    if let Err(e) = client.ping().await {
                        warn!("MQTT ping failed: {:?}", e);
                        break 'session;
                    }
                }
            }
        }

        MQTT_CONNECTED.store(false, Ordering::Relaxed);
        Timer::after(RECONNECT_DELAY).await;
    }
}

async fn subscribe_topic(
    client: &mut DoorsysMqttClient<'_, '_, TcpSocket<'_>>,
    topic: &str,
) -> Result<(), ()> {
    let topic_str = MqttString::try_from(topic).map_err(|_| ())?;
    let filter = TopicFilter::new(topic_str).ok_or(())?;
    let sub_opts = SubscriptionOptions::new()
        .retain_handling(RetainHandling::SendIfNotSubscribedBefore)
        .at_least_once();

    if let Err(e) = client.subscribe(filter, &sub_opts).await {
        error!("Failed to send SUBSCRIBE for {}: {:?}", topic, e);
        return Err(());
    }

    match client.poll().await {
        Ok(Event::Suback(Suback { reason_code, .. })) => {
            info!("Subscribed to {} ({:?})", topic, reason_code);
            Ok(())
        }
        Ok(other) => {
            warn!("Unexpected event while waiting for SUBACK: {:?}", other);
            Ok(())
        }
        Err(e) => {
            error!("Error waiting for SUBACK on {}: {:?}", topic, e);
            Err(())
        }
    }
}

async fn handle_event<H>(
    event: Event<'_, 0, 0>,
    pending_qos1: &mut Option<u16>,
    on_message: &mut H,
) -> bool
where
    H: AsyncFnMut(&str, &[u8]),
{
    match event {
        Event::Publish(pub_pkt) => {
            if let Some(topic_name) = pub_pkt.topic.name() {
                let topic_str: &str = topic_name.as_ref().as_ref();
                let payload: &[u8] = &pub_pkt.message;
                info!(
                    "MQTT message received on {}: {} bytes",
                    topic_str,
                    payload.len()
                );
                on_message(topic_str, payload).await;
            }
            true
        }
        Event::PublishAcknowledged(ack) | Event::PublishComplete(ack) => {
            if pending_qos1 == &Some(ack.packet_identifier.get().get()) {
                *pending_qos1 = None;
            }
            true
        }
        Event::PublishRejected(rej) => {
            warn!(
                "MQTT publish rejected (id={}, reason={:?})",
                rej.packet_identifier.get(),
                rej.reason_code
            );
            if pending_qos1 == &Some(rej.packet_identifier.get().get()) {
                *pending_qos1 = None;
            }
            true
        }
        Event::Pingresp => true,
        other => {
            info!("MQTT event: {:?}", other);
            false
        }
    }
}
