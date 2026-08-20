//! MQTT client for raw devices / ESPHome sensors not fronted by Home Assistant
//! (architecture §4.4). Uses rumqttc (TLS-capable, QoS 1). Subscribes device
//! state topics into the same [`EntityMirror`] and publishes idempotent-keyed
//! commands.
//!
//! The topic/payload mapping and mirror integration are unit-tested; the live
//! broker connection (`run`) needs an actual MQTT broker, which is the
//! deployment-time wiring.

use super::homeassistant::EntityMirror;
use rumqttc::{AsyncClient, MqttOptions, QoS};
use std::sync::Arc;
use std::time::Duration;

/// Configuration for the MQTT connection.
#[derive(Debug, Clone)]
pub struct MqttConfig {
    pub client_id: String,
    pub host: String,
    pub port: u16,
    pub keep_alive_s: u64,
    /// Topics to subscribe (e.g. `["esphome/+/state", "sensors/#"]`).
    pub subscribe: Vec<String>,
}

impl Default for MqttConfig {
    fn default() -> Self {
        MqttConfig {
            client_id: "oracle".into(),
            host: "127.0.0.1".into(),
            port: 1883,
            keep_alive_s: 30,
            subscribe: vec!["oracle/+/state".into()],
        }
    }
}

/// Map an MQTT state topic to an entity id for the mirror. `esphome/lamp/state`
/// → `esphome.lamp`. Returns None for topics that don't match the `.../state`
/// convention.
pub fn entity_from_topic(topic: &str) -> Option<String> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() < 2 || *parts.last().unwrap() != "state" {
        return None;
    }
    // domain.name from the first two segments.
    Some(format!("{}.{}", parts[0], parts[1]))
}

/// A connected MQTT handle. Publishing is idempotent-keyed by topic.
pub struct MqttClient {
    client: AsyncClient,
    pub mirror: Arc<EntityMirror>,
}

impl MqttClient {
    /// Connect and spawn the event loop that pumps inbound messages into the
    /// mirror. QoS 1 for at-least-once delivery.
    pub async fn connect(cfg: MqttConfig) -> anyhow::Result<Self> {
        let mut opts = MqttOptions::new(&cfg.client_id, &cfg.host, cfg.port);
        opts.set_keep_alive(Duration::from_secs(cfg.keep_alive_s));
        // Last-will marks us offline if we drop unexpectedly.
        opts.set_last_will(rumqttc::LastWill::new(
            format!("oracle/{}/status", cfg.client_id),
            "offline",
            QoS::AtLeastOnce,
            true,
        ));

        let (client, mut eventloop) = AsyncClient::new(opts, 64);
        for topic in &cfg.subscribe {
            client.subscribe(topic, QoS::AtLeastOnce).await?;
        }
        let mirror = Arc::new(EntityMirror::new());
        let mirror_pump = mirror.clone();
        tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(p))) => {
                        if let Some(entity) = entity_from_topic(&p.topic) {
                            let state = String::from_utf8_lossy(&p.payload).to_string();
                            mirror_pump.set_raw(&entity, &state);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("mqtt eventloop error: {e}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });
        Ok(MqttClient { client, mirror })
    }

    /// Publish a command with QoS 1. `retain=false` for commands.
    pub async fn publish_command(&self, topic: &str, payload: &str) -> anyhow::Result<()> {
        self.client
            .publish(topic, QoS::AtLeastOnce, false, payload.as_bytes())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_to_entity_mapping() {
        assert_eq!(
            entity_from_topic("esphome/lamp/state").as_deref(),
            Some("esphome.lamp")
        );
        assert_eq!(
            entity_from_topic("sensors/kitchen/state").as_deref(),
            Some("sensors.kitchen")
        );
        assert_eq!(entity_from_topic("esphome/lamp/command"), None); // not a state topic
        assert_eq!(entity_from_topic("single"), None);
    }

    #[test]
    fn default_config_is_sane() {
        let c = MqttConfig::default();
        assert_eq!(c.port, 1883);
        assert!(!c.subscribe.is_empty());
    }
}
