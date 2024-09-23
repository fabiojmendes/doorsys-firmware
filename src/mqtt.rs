use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use doorsys_protocol::UserAction;
use esp_idf_svc::mqtt::client::{
    Details, EspMqttClient, EspMqttConnection, EventPayload, MqttClientConfiguration, QoS,
};

use crate::config::MqttConfig;
use crate::user::UserDB;

pub type MqttClient = EspMqttClient<'static>;

pub fn setup_mqtt(
    net_id: &str,
    user_db: UserDB,
    config: &MqttConfig,
) -> anyhow::Result<Arc<Mutex<MqttClient>>> {
    let mqtt_config = MqttClientConfiguration {
        client_id: Some(net_id),
        username: Some(&config.username),
        password: Some(&config.password),
        disable_clean_session: true,
        ..Default::default()
    };

    let (sub_sender, sub_receiver) = mpsc::channel();

    let (client, conn) = EspMqttClient::new(&config.url, &mqtt_config)?;
    connection_polling_thread(conn, sub_sender, user_db);

    let client = Arc::new(Mutex::new(client));
    subscriber_thread(client.clone(), sub_receiver);
    Ok(client)
}

fn connection_polling_thread(
    mut conn: EspMqttConnection,
    sub_sender: mpsc::Sender<()>,
    user_db: UserDB,
) {
    thread::spawn(move || {
        let mut shared_buf = Vec::new();
        let mut shared_topic = String::new();
        while let Ok(event) = conn.next() {
            match event.payload() {
                EventPayload::Received {
                    id: _,
                    topic,
                    data,
                    details,
                } => {
                    log::info!(
                        "Message received {:?} {:?}, {} bytes",
                        topic,
                        details,
                        data.len()
                    );
                    let (topic, data) = match details {
                        Details::InitialChunk(init) => {
                            shared_buf = Vec::with_capacity(init.total_data_size);
                            shared_buf.extend_from_slice(data);
                            shared_topic = String::from(topic.unwrap());
                            continue;
                        }
                        Details::SubsequentChunk(_sub) => {
                            shared_buf.extend_from_slice(data);
                            if shared_buf.len() != shared_buf.capacity() {
                                continue;
                            }
                            (shared_topic.as_str(), shared_buf.as_slice())
                        }
                        Details::Complete => (topic.unwrap(), data),
                    };
                    route_message(topic, data, &user_db);
                }
                EventPayload::Connected(session) => {
                    log::info!("Connected session = {session}");
                    sub_sender.send(()).unwrap();
                }
                EventPayload::Error(e) => log::error!("from mqtt: {:?}", e),
                event => log::info!("mqtt event: {:?}", event),
            }
        }
    });
}

fn subscriber_thread(
    client: Arc<Mutex<EspMqttClient<'static>>>,
    conn_receiver: mpsc::Receiver<()>,
) {
    thread::spawn(move || {
        while conn_receiver.recv().is_ok() {
            let topic = "doorsys/user";
            match client.lock().unwrap().subscribe(topic, QoS::AtLeastOnce) {
                Ok(id) => log::info!("Subscribed to {topic} {id}"),
                Err(e) => log::error!("Failed to subscribe to topic {topic}: {e}"),
            };
        }
    });
}

fn route_message(topic: &str, data: &[u8], user_db: &UserDB) {
    match topic {
        "doorsys/user" => process_user_message(data, user_db),
        _ => log::warn!("unknown topic {}", topic),
    };
}

fn process_user_message(data: &[u8], user_db: &UserDB) {
    match postcard::from_bytes(data) {
        Ok(UserAction::Add(code)) => {
            log::info!("Adding code {}", code);
            if let Err(e) = user_db.add(code) {
                log::error!("Error adding new code {}", e);
            }
        }
        Ok(UserAction::Del(code)) => {
            log::info!("Deleting code {}", code);
            if let Err(e) = user_db.delete(code) {
                log::error!("Error deleting code {}", e);
            }
        }
        Ok(UserAction::Replace { old, new }) => {
            log::info!("Replacing code {} with {}", old, new);
            if let Err(e) = user_db.replace(old, new) {
                log::error!("Error replacing code {}", e);
            }
        }
        Ok(UserAction::Bulk(codes)) => {
            log::info!("Bulk adding codes {}", codes.len());
            if let Err(e) = user_db.bulk(codes) {
                log::error!("Error bulk inserting codes {}", e);
            }
        }
        Err(e) => {
            log::error!("decoding error: {}", e);
        }
    };
}
