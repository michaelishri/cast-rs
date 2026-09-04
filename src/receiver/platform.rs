use std::sync::Mutex;
use std::sync::mpsc::Sender;

use serde_json::{Value, json};

use crate::discovery::DeviceCapability;
use rust_cast::message_manager::{CastMessage, CastMessagePayload};

use super::ReceiverEvent;
use super::server;

pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
pub const NS_DEVICEAUTH: &str = "urn:x-cast:com.google.cast.tp.deviceauth";
pub const RECEIVER_ID: &str = "receiver-0";

pub const DEFAULT_MEDIA_RECEIVER_APP_ID: &str = "CC1AD845";
const DEFAULT_MEDIA_RECEIVER_NAME: &str = "Default Media Receiver";
const VOLUME_STEP_INTERVAL: f64 = 0.05;

pub struct CoreConfig {
    pub name: String,
    pub model: String,
    pub capability: DeviceCapability,
    pub events: Sender<super::ReceiverEvent>,
}

pub struct ReceiverCore {
    name: String,
    model: String,
    capability: DeviceCapability,
    events: Sender<super::ReceiverEvent>,
    state: Mutex<CoreState>,
}

struct CoreState {
    volume_level: f64,
    volume_muted: bool,
    session: Option<Session>,
    connected_senders: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub session_id: String,
    pub transport_id: String,
    pub app_id: String,
}

impl ReceiverCore {
    pub fn new(config: CoreConfig) -> Self {
        Self {
            name: config.name,
            model: config.model,
            capability: config.capability,
            events: config.events,
            state: Mutex::new(CoreState {
                volume_level: 1.0,
                volume_muted: false,
                session: None,
                connected_senders: Vec::new(),
            }),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn capability(&self) -> DeviceCapability {
        self.capability
    }

    /// Emits a lifecycle event on the receiver event channel.
    pub fn notify(&self, event: super::ReceiverEvent) {
        self.events.send(event).ok();
    }

    /// Handles one inbound Cast protocol message and returns the outgoing
    /// dispatches it triggers.
    pub fn handle_inbound(&self, _conn: u64, message: &CastMessage) -> Vec<server::Dispatch> {
        match message.namespace.as_str() {
            NS_CONNECTION => self.handle_connection(message),
            NS_HEARTBEAT => self.handle_heartbeat(message),
            NS_RECEIVER => self.handle_receiver(message),
            NS_MEDIA => self.handle_media(message),
            // Device auth is answered directly by the server with the
            // receiver's own certificate chain.
            _ => Vec::new(),
        }
    }

    fn handle_connection(&self, message: &CastMessage) -> Vec<server::Dispatch> {
        let Some(payload) = message_payload_json(message) else {
            return Vec::new();
        };
        match payload.get("type").and_then(Value::as_str) {
            Some("CONNECT") => {
                let mut state = self.state.lock().expect("receiver core state");
                if !state
                    .connected_senders
                    .iter()
                    .any(|id| id == &message.source)
                {
                    state.connected_senders.push(message.source.clone());
                }
                Vec::new()
            }
            Some("CLOSE") => {
                let sender = message.source.clone();
                let mut state = self.state.lock().expect("receiver core state");
                state.connected_senders.retain(|id| id != &sender);
                let session_closes = state
                    .session
                    .as_ref()
                    .is_some_and(|session| session.transport_id == sender);
                if session_closes {
                    state.session = None;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_heartbeat(&self, message: &CastMessage) -> Vec<server::Dispatch> {
        let Some(payload) = message_payload_json(message) else {
            return Vec::new();
        };
        match payload.get("type").and_then(Value::as_str) {
            Some("PING") => vec![server::Dispatch::Reply {
                message: json_message(
                    NS_HEARTBEAT,
                    RECEIVER_ID,
                    &message.source,
                    json!({"type": "PONG"}),
                ),
            }],
            _ => Vec::new(),
        }
    }

    fn handle_receiver(&self, message: &CastMessage) -> Vec<server::Dispatch> {
        let Some(payload) = message_payload_json(message) else {
            return Vec::new();
        };
        let sender = message.source.clone();
        let request_id = payload.get("requestId").and_then(Value::as_u64);
        match payload.get("type").and_then(Value::as_str) {
            Some("GET_STATUS") => vec![self.status_reply(&sender, request_id)],
            Some("SET_VOLUME") => self.set_volume(&sender, request_id, &payload),
            Some("LAUNCH") => self.launch(&sender, request_id, &payload),
            Some("GET_APP_AVAILABILITY") => self.app_availability(&sender, request_id, &payload),
            Some("STOP") => self.stop_session(&sender, request_id, &payload),
            _ => Vec::new(),
        }
    }

    fn handle_media(&self, message: &CastMessage) -> Vec<server::Dispatch> {
        let Some(payload) = message_payload_json(message) else {
            return Vec::new();
        };
        let sender = message.source.clone();
        let request_id = payload.get("requestId").and_then(Value::as_u64);
        let message_type = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match message_type {
            // Playback arrives with ticket #84; the skeleton rejects loads
            // explicitly so senders surface a diagnosable failure.
            "LOAD" => {
                self.notify(ReceiverEvent::LoadRejected {
                    sender: sender.clone(),
                });
                vec![reply(
                    NS_MEDIA,
                    message,
                    json!({"type": "LOAD_FAILED", "requestId": request_id.unwrap_or(0)}),
                )]
            }
            "GET_STATUS" => vec![reply(
                NS_MEDIA,
                message,
                json!({"type": "MEDIA_STATUS", "requestId": request_id.unwrap_or(0), "status": []}),
            )],
            _ => vec![reply(
                NS_MEDIA,
                message,
                json!({"type": "INVALID_REQUEST", "requestId": request_id.unwrap_or(0)}),
            )],
        }
    }

    fn status_reply(&self, sender: &str, request_id: Option<u64>) -> server::Dispatch {
        let mut reply = self.receiver_status(request_id);
        reply.destination = sender.to_owned();
        server::Dispatch::Reply { message: reply }
    }

    fn receiver_status(&self, request_id: Option<u64>) -> CastMessage {
        let state = self.state.lock().expect("receiver core state");
        let mut payload = serde_json::Map::new();
        payload.insert("type".into(), Value::String("RECEIVER_STATUS".into()));
        if let Some(request_id) = request_id {
            payload.insert("requestId".into(), Value::from(request_id));
        }
        payload.insert("status".into(), self.status_body(&state));
        json_message(NS_RECEIVER, RECEIVER_ID, "*", Value::Object(payload))
    }

    fn status_body(&self, state: &CoreState) -> Value {
        let applications = match &state.session {
            Some(session) => vec![json!({
                "appId": session.app_id,
                "displayName": DEFAULT_MEDIA_RECEIVER_NAME,
                "isIdleScreen": false,
                "sessionId": session.session_id,
                "statusText": DEFAULT_MEDIA_RECEIVER_NAME,
                "transportId": session.transport_id,
                "namespaces": [{"name": NS_MEDIA}],
            })],
            None => Vec::new(),
        };
        json!({
            "applications": applications,
            "volume": {
                "controlType": "master",
                "level": state.volume_level,
                "muted": state.volume_muted,
                "stepInterval": VOLUME_STEP_INTERVAL,
            },
            "isActiveInput": state.session.is_some(),
            "standby": state.session.is_none(),
        })
    }

    fn set_volume(
        &self,
        sender: &str,
        request_id: Option<u64>,
        payload: &Value,
    ) -> Vec<server::Dispatch> {
        {
            let mut state = self.state.lock().expect("receiver core state");
            if let Some(volume) = payload.get("volume") {
                if let Some(level) = volume.get("level").and_then(Value::as_f64) {
                    state.volume_level = level.clamp(0.0, 1.0);
                }
                if let Some(muted) = volume.get("muted").and_then(Value::as_bool) {
                    state.volume_muted = muted;
                }
                let (level, muted) = (state.volume_level, state.volume_muted);
                drop(state);
                self.notify(ReceiverEvent::VolumeChanged { level, muted });
            }
        }
        vec![
            self.status_reply(sender, request_id),
            self.broadcast_status(),
        ]
    }

    fn launch(
        &self,
        sender: &str,
        request_id: Option<u64>,
        payload: &Value,
    ) -> Vec<server::Dispatch> {
        let app_id = payload
            .get("appId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if app_id != DEFAULT_MEDIA_RECEIVER_APP_ID {
            return vec![reply_to(
                NS_RECEIVER,
                RECEIVER_ID,
                sender,
                json!({"type": "LAUNCH_ERROR", "requestId": request_id.unwrap_or(0), "reason": "NOT_FOUND"}),
            )];
        }
        {
            let mut state = self.state.lock().expect("receiver core state");
            if state.session.is_none() {
                state.session = Some(Session {
                    session_id: random_hex(16),
                    transport_id: format!("receiver-{}", random_hex(8)),
                    app_id,
                });
            }
        }
        self.notify(ReceiverEvent::Launched {
            app_id: DEFAULT_MEDIA_RECEIVER_APP_ID.to_owned(),
            sender: sender.to_owned(),
        });
        vec![
            self.status_reply(sender, request_id),
            self.broadcast_status(),
        ]
    }

    fn app_availability(
        &self,
        sender: &str,
        request_id: Option<u64>,
        payload: &Value,
    ) -> Vec<server::Dispatch> {
        let requested = payload
            .get("appId")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let availability: Vec<Value> = requested
            .iter()
            .map(|app| {
                let app_id = app.as_str().unwrap_or_default();
                json!({
                    "appId": app_id,
                    "available": app_id == DEFAULT_MEDIA_RECEIVER_APP_ID,
                })
            })
            .collect();
        vec![reply_to(
            NS_RECEIVER,
            RECEIVER_ID,
            sender,
            json!({
                "type": "GET_APP_AVAILABILITY_RESPONSE",
                "requestId": request_id.unwrap_or(0),
                "availability": availability,
            }),
        )]
    }

    fn stop_session(
        &self,
        sender: &str,
        request_id: Option<u64>,
        payload: &Value,
    ) -> Vec<server::Dispatch> {
        let target = payload.get("sessionId").and_then(Value::as_str);
        let stopped = {
            let mut state = self.state.lock().expect("receiver core state");
            let matches = match (&state.session, target) {
                (Some(session), Some(session_id)) => session.session_id == session_id,
                // A STOP without a session id targets whatever is running.
                (Some(_), None) => true,
                _ => false,
            };
            if matches {
                state.session = None;
                true
            } else {
                false
            }
        };
        if stopped {
            self.notify(ReceiverEvent::SessionStopped {
                sender: sender.to_owned(),
            });
            vec![
                self.status_reply(sender, request_id),
                self.broadcast_status(),
            ]
        } else {
            vec![self.status_reply(sender, request_id)]
        }
    }

    fn broadcast_status(&self) -> server::Dispatch {
        server::Dispatch::Broadcast {
            message: self.receiver_status(None),
        }
    }
}

fn json_message(namespace: &str, source: &str, destination: &str, payload: Value) -> CastMessage {
    CastMessage {
        namespace: namespace.to_owned(),
        source: source.to_owned(),
        destination: destination.to_owned(),
        payload: CastMessagePayload::String(payload.to_string()),
    }
}

fn reply_to(namespace: &str, source: &str, destination: &str, payload: Value) -> server::Dispatch {
    server::Dispatch::Reply {
        message: json_message(namespace, source, destination, payload),
    }
}

fn reply(namespace: &str, inbound: &CastMessage, payload: Value) -> server::Dispatch {
    reply_to(namespace, RECEIVER_ID, &inbound.source, payload)
}

fn message_payload_json(message: &CastMessage) -> Option<Value> {
    match &message.payload {
        CastMessagePayload::String(payload) => serde_json::from_str(payload).ok(),
        CastMessagePayload::Binary(_) => None,
    }
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0_u8; bytes];
    getrandom::fill(&mut buffer).expect("randomness is always available");
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use rust_cast::message_manager::CastMessagePayload;

    use super::*;

    fn core() -> (ReceiverCore, mpsc::Receiver<ReceiverEvent>) {
        let (events, rx) = mpsc::channel();
        let core = ReceiverCore::new(CoreConfig {
            name: "Test Cast".to_owned(),
            model: "Cast Desktop Receiver".to_owned(),
            capability: DeviceCapability::Video,
            events,
        });
        (core, rx)
    }

    fn inbound(namespace: &str, payload: &str) -> CastMessage {
        CastMessage {
            namespace: namespace.to_owned(),
            source: "sender-0".to_owned(),
            destination: RECEIVER_ID.to_owned(),
            payload: CastMessagePayload::String(payload.to_owned()),
        }
    }

    fn payload_of(dispatch: &server::Dispatch) -> Value {
        let server::Dispatch::Reply { message } = dispatch else {
            panic!("expected a reply dispatch");
        };
        match &message.payload {
            CastMessagePayload::String(payload) => serde_json::from_str(payload).unwrap(),
            CastMessagePayload::Binary(_) => panic!("expected a JSON payload"),
        }
    }

    #[test]
    fn core_exposes_the_advertised_metadata() {
        let (core, _) = core();
        assert_eq!(core.name(), "Test Cast");
        assert_eq!(core.model(), "Cast Desktop Receiver");
        assert_eq!(core.capability(), DeviceCapability::Video);
    }

    #[test]
    fn get_status_reports_idle_with_master_volume() {
        let (core, _) = core();
        let dispatches = core.handle_inbound(
            1,
            &inbound(NS_RECEIVER, r#"{"type":"GET_STATUS","requestId":7}"#),
        );
        assert_eq!(dispatches.len(), 1);
        let payload = payload_of(&dispatches[0]);
        assert_eq!(payload["type"], "RECEIVER_STATUS");
        assert_eq!(payload["requestId"], 7);
        assert_eq!(payload["status"]["applications"], json!([]));
        assert_eq!(payload["status"]["volume"]["controlType"], "master");
        assert_eq!(payload["status"]["volume"]["level"], 1.0);
        assert_eq!(payload["status"]["standby"], true);
    }

    #[test]
    fn heartbeat_pings_are_answered_with_pongs() {
        let (core, _) = core();
        let dispatches = core.handle_inbound(1, &inbound(NS_HEARTBEAT, r#"{"type":"PING"}"#));
        assert_eq!(dispatches.len(), 1);
        assert_eq!(payload_of(&dispatches[0])["type"], "PONG");
    }

    #[test]
    fn launch_creates_a_default_media_receiver_session() {
        let (core, events) = core();
        let dispatches = core.handle_inbound(
            1,
            &inbound(
                NS_RECEIVER,
                r#"{"type":"LAUNCH","appId":"CC1AD845","requestId":3}"#,
            ),
        );
        let reply = payload_of(&dispatches[0]);
        assert_eq!(reply["type"], "RECEIVER_STATUS");
        let app = &reply["status"]["applications"][0];
        assert_eq!(app["appId"], "CC1AD845");
        assert_eq!(app["displayName"], "Default Media Receiver");
        assert!(app["sessionId"].as_str().is_some_and(|id| id.len() == 32));
        assert!(
            app["transportId"]
                .as_str()
                .unwrap()
                .starts_with("receiver-")
        );
        assert_eq!(reply["status"]["standby"], false);
        assert!(matches!(
            events.try_recv(),
            Ok(ReceiverEvent::Launched { ref app_id, ref sender })
                if app_id == "CC1AD845" && sender == "sender-0"
        ));
    }

    #[test]
    fn launching_an_unsupported_app_is_a_not_found_error() {
        let (core, _) = core();
        let dispatches = core.handle_inbound(
            1,
            &inbound(
                NS_RECEIVER,
                r#"{"type":"LAUNCH","appId":"233637DE","requestId":4}"#,
            ),
        );
        let payload = payload_of(&dispatches[0]);
        assert_eq!(payload["type"], "LAUNCH_ERROR");
        assert_eq!(payload["requestId"], 4);
        assert_eq!(payload["reason"], "NOT_FOUND");
    }

    fn status_session_id(core: &ReceiverCore) -> String {
        let dispatches = core.handle_inbound(1, &inbound(NS_RECEIVER, r#"{"type":"GET_STATUS"}"#));
        let payload = payload_of(&dispatches[0]);
        payload["status"]["applications"][0]["sessionId"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn relaunching_the_running_app_is_idempotent() {
        let (core, _) = core();
        core.handle_inbound(
            1,
            &inbound(NS_RECEIVER, r#"{"type":"LAUNCH","appId":"CC1AD845"}"#),
        );
        let first = status_session_id(&core);
        core.handle_inbound(
            1,
            &inbound(NS_RECEIVER, r#"{"type":"LAUNCH","appId":"CC1AD845"}"#),
        );
        let second = status_session_id(&core);
        assert_eq!(first, second);
    }

    #[test]
    fn stop_ends_the_session_and_reports_idle() {
        let (core, events) = core();
        let dispatches = core.handle_inbound(
            1,
            &inbound(NS_RECEIVER, r#"{"type":"LAUNCH","appId":"CC1AD845"}"#),
        );
        let session_id = payload_of(&dispatches[0])["status"]["applications"][0]["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let dispatches = core.handle_inbound(
            1,
            &inbound(
                NS_RECEIVER,
                &format!(r#"{{"type":"STOP","sessionId":"{session_id}","requestId":9}}"#),
            ),
        );
        let reply = payload_of(&dispatches[0]);
        assert_eq!(reply["type"], "RECEIVER_STATUS");
        assert_eq!(reply["requestId"], 9);
        assert_eq!(reply["status"]["applications"], json!([]));
        // The launch event precedes the stop event; drain until the stop.
        let stopped = std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, ReceiverEvent::SessionStopped { .. }));
        assert!(stopped);
    }

    #[test]
    fn stopping_an_unknown_session_keeps_the_current_one() {
        let (core, _) = core();
        core.handle_inbound(
            1,
            &inbound(NS_RECEIVER, r#"{"type":"LAUNCH","appId":"CC1AD845"}"#),
        );
        core.handle_inbound(
            1,
            &inbound(NS_RECEIVER, r#"{"type":"STOP","sessionId":"nope"}"#),
        );
        let dispatches = core.handle_inbound(1, &inbound(NS_RECEIVER, r#"{"type":"GET_STATUS"}"#));
        let status = payload_of(&dispatches[0]);
        assert_eq!(
            status["status"]["applications"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn set_volume_updates_level_and_mute_and_broadcasts() {
        let (core, events) = core();
        let dispatches = core.handle_inbound(
            1,
            &inbound(
                NS_RECEIVER,
                r#"{"type":"SET_VOLUME","requestId":5,"volume":{"level":0.4}}"#,
            ),
        );
        assert!(matches!(dispatches[0], server::Dispatch::Reply { .. }));
        assert!(matches!(dispatches[1], server::Dispatch::Broadcast { .. }));
        let status = payload_of(&dispatches[0]);
        assert_eq!(status["status"]["volume"]["level"], 0.4);
        assert!(matches!(
            events.try_recv(),
            Ok(ReceiverEvent::VolumeChanged {
                level: 0.4,
                muted: false
            })
        ));

        core.handle_inbound(
            1,
            &inbound(
                NS_RECEIVER,
                r#"{"type":"SET_VOLUME","volume":{"muted":true}}"#,
            ),
        );
        let dispatches = core.handle_inbound(1, &inbound(NS_RECEIVER, r#"{"type":"GET_STATUS"}"#));
        let status = payload_of(&dispatches[0]);
        assert_eq!(status["status"]["volume"]["muted"], true);

        // Levels are clamped into [0, 1].
        core.handle_inbound(
            1,
            &inbound(
                NS_RECEIVER,
                r#"{"type":"SET_VOLUME","volume":{"level":42}}"#,
            ),
        );
        let dispatches = core.handle_inbound(1, &inbound(NS_RECEIVER, r#"{"type":"GET_STATUS"}"#));
        let status = payload_of(&dispatches[0]);
        assert_eq!(status["status"]["volume"]["level"], 1.0);
    }

    #[test]
    fn app_availability_only_offers_the_default_media_receiver() {
        let (core, _) = core();
        let dispatches = core.handle_inbound(
            1,
            &inbound(
                NS_RECEIVER,
                r#"{"type":"GET_APP_AVAILABILITY","requestId":6,"appId":["CC1AD845","233637DE"]}"#,
            ),
        );
        let payload = payload_of(&dispatches[0]);
        assert_eq!(payload["type"], "GET_APP_AVAILABILITY_RESPONSE");
        assert_eq!(payload["requestId"], 6);
        assert_eq!(payload["availability"][0]["available"], true);
        assert_eq!(payload["availability"][1]["available"], false);
    }

    #[test]
    fn loads_are_rejected_while_playback_is_pending() {
        let (core, events) = core();
        let dispatches = core.handle_inbound(
            1,
            &inbound(NS_MEDIA, r#"{"type":"LOAD","requestId":11,"media":{"contentId":"http://example.invalid/movie.mp4"}}"#),
        );
        let payload = payload_of(&dispatches[0]);
        assert_eq!(payload["type"], "LOAD_FAILED");
        assert_eq!(payload["requestId"], 11);
        assert!(matches!(
            events.try_recv(),
            Ok(ReceiverEvent::LoadRejected { .. })
        ));
    }

    #[test]
    fn media_status_with_no_session_is_an_empty_status_array() {
        let (core, _) = core();
        let dispatches = core.handle_inbound(
            1,
            &inbound(NS_MEDIA, r#"{"type":"GET_STATUS","requestId":2}"#),
        );
        let payload = payload_of(&dispatches[0]);
        assert_eq!(payload["type"], "MEDIA_STATUS");
        assert_eq!(payload["status"], serde_json::json!([]));
    }

    #[test]
    fn other_media_commands_are_invalid_requests() {
        let (core, _) = core();
        for command in ["PLAY", "PAUSE", "SEEK", "STOP", "QUEUE_LOAD"] {
            let dispatches = core.handle_inbound(
                1,
                &inbound(
                    NS_MEDIA,
                    &format!(r#"{{"type":"{command}","requestId":1}}"#),
                ),
            );
            assert_eq!(payload_of(&dispatches[0])["type"], "INVALID_REQUEST");
        }
    }

    #[test]
    fn connection_tracking_follows_connect_and_close() {
        let (core, _) = core();
        core.handle_inbound(1, &inbound(NS_CONNECTION, r#"{"type":"CONNECT"}"#));
        assert!(
            core.state
                .lock()
                .unwrap()
                .connected_senders
                .contains(&"sender-0".to_owned())
        );
        core.handle_inbound(1, &inbound(NS_CONNECTION, r#"{"type":"CLOSE"}"#));
        assert!(core.state.lock().unwrap().connected_senders.is_empty());
    }

    #[test]
    fn unknown_namespaces_are_ignored() {
        let (core, _) = core();
        let dispatches = core.handle_inbound(
            1,
            &inbound("urn:x-cast:com.example.custom", r#"{"type":"PING"}"#),
        );
        assert!(dispatches.is_empty());
    }

    #[test]
    fn malformed_json_payloads_are_ignored() {
        let (core, _) = core();
        for namespace in [NS_CONNECTION, NS_HEARTBEAT, NS_RECEIVER, NS_MEDIA] {
            let dispatches = core.handle_inbound(1, &inbound(namespace, "not json at all"));
            assert!(
                dispatches.is_empty(),
                "namespace {namespace} should ignore malformed payloads"
            );
        }
    }
}
