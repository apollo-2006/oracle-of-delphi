//! Home Assistant WebSocket + a mirrored entity-state table (architecture §4.4).
//!
//! HA's WebSocket API is a small JSON protocol: `auth` handshake, then
//! `subscribe_events` for pushed `state_changed`, and `call_service` for
//! mutations. We keep a local mirror of entity states so *reads are instant*
//! and only *writes* round-trip. The message builders/parsers and the mirror
//! are pure and unit-tested; the socket itself is a thin wrapper.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

/// Outbound message builders. HA requires a monotonically increasing `id` on
/// every command after auth.
pub fn auth_message(access_token: &str) -> Value {
    json!({ "type": "auth", "access_token": access_token })
}

pub fn subscribe_state_changes(id: u64) -> Value {
    json!({ "id": id, "type": "subscribe_events", "event_type": "state_changed" })
}

pub fn call_service(id: u64, domain: &str, service: &str, entity_id: &str, data: Value) -> Value {
    let mut service_data = json!({ "entity_id": entity_id });
    if let Value::Object(extra) = data {
        if let Value::Object(base) = &mut service_data {
            base.extend(extra);
        }
    }
    json!({
        "id": id,
        "type": "call_service",
        "domain": domain,
        "service": service,
        "service_data": service_data,
    })
}

/// Convenience: set a light's brightness (0-100%) with a 1s transition.
pub fn light_set(id: u64, entity_id: &str, brightness_pct: u8) -> Value {
    if brightness_pct == 0 {
        return call_service(id, "light", "turn_off", entity_id, json!({}));
    }
    call_service(
        id,
        "light",
        "turn_on",
        entity_id,
        json!({ "brightness_pct": brightness_pct.min(100), "transition": 1 }),
    )
}

/// The locally-mirrored entity state table.
#[derive(Default)]
pub struct EntityMirror {
    states: Mutex<HashMap<String, String>>,
}

impl EntityMirror {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply an inbound HA message. Returns `Some((entity, new_state))` if it
    /// was a `state_changed` event we recorded.
    pub fn apply(&self, msg: &Value) -> Option<(String, String)> {
        if msg.get("type")?.as_str()? != "event" {
            return None;
        }
        let ev = msg.get("event")?;
        if ev.get("event_type")?.as_str()? != "state_changed" {
            return None;
        }
        let data = ev.get("data")?;
        let entity = data.get("entity_id")?.as_str()?.to_string();
        let new_state = data.get("new_state")?.get("state")?.as_str()?.to_string();
        self.states
            .lock()
            .unwrap()
            .insert(entity.clone(), new_state.clone());
        Some((entity, new_state))
    }

    /// Directly set an entity's state (used by the MQTT pump, whose topics
    /// aren't HA `state_changed` events but map to the same mirror).
    pub fn set_raw(&self, entity_id: &str, state: &str) {
        self.states
            .lock()
            .unwrap()
            .insert(entity_id.to_string(), state.to_string());
    }

    /// Instant local read (no round-trip).
    pub fn get(&self, entity_id: &str) -> Option<String> {
        self.states.lock().unwrap().get(entity_id).cloned()
    }

    pub fn len(&self) -> usize {
        self.states.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_and_subscribe_shapes() {
        assert_eq!(auth_message("tok")["type"], "auth");
        let s = subscribe_state_changes(3);
        assert_eq!(s["id"], 3);
        assert_eq!(s["event_type"], "state_changed");
    }

    #[test]
    fn light_set_full_brightness() {
        let m = light_set(5, "light.bedroom", 30);
        assert_eq!(m["domain"], "light");
        assert_eq!(m["service"], "turn_on");
        assert_eq!(m["service_data"]["brightness_pct"], 30);
        assert_eq!(m["service_data"]["entity_id"], "light.bedroom");
        assert_eq!(m["service_data"]["transition"], 1);
    }

    #[test]
    fn light_set_zero_turns_off() {
        let m = light_set(6, "light.bedroom", 0);
        assert_eq!(m["service"], "turn_off");
    }

    #[test]
    fn mirror_records_state_changes() {
        let mirror = EntityMirror::new();
        let ev = json!({
            "type": "event",
            "event": {
                "event_type": "state_changed",
                "data": {
                    "entity_id": "light.bedroom",
                    "new_state": {"state": "on"}
                }
            }
        });
        let applied = mirror.apply(&ev);
        assert_eq!(applied, Some(("light.bedroom".into(), "on".into())));
        assert_eq!(mirror.get("light.bedroom").as_deref(), Some("on"));
    }

    #[test]
    fn mirror_ignores_non_state_events() {
        let mirror = EntityMirror::new();
        assert!(mirror
            .apply(&json!({"type":"result","success":true}))
            .is_none());
        assert!(mirror.is_empty());
    }
}
