use std::path::Path as StdPath;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::discovery::DeviceCapability;
use rust_cast::message_manager::{CastMessage, CastMessagePayload};

use super::ReceiverEvent;
use super::decode::{self, FrameSlot};
use super::fetch;
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
const SUPPORTED_MEDIA_COMMANDS: u32 = 15;
/// How long an ended media session keeps the app running before the
/// receiver returns to idle, mirroring the Default Media Receiver.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct CoreConfig {
    pub name: String,
    pub model: String,
    pub capability: DeviceCapability,
    pub events: Sender<ReceiverEvent>,
    /// Video sink shared with decode threads; None disables media playback.
    pub video: Option<FrameSlot>,
    pub window_enabled: bool,
}

pub struct ReceiverCore {
    name: String,
    model: String,
    capability: DeviceCapability,
    events: Sender<ReceiverEvent>,
    video: Option<FrameSlot>,
    window_enabled: bool,
    state: Mutex<CoreState>,
}

struct CoreState {
    volume_level: f64,
    volume_muted: bool,
    session: Option<Session>,
    connected_senders: Vec<String>,
    media: Option<MediaPlayback>,
    next_media_session_id: i32,
}

/// Everything the media-namespace statuses need about the active playback.
struct MediaPlayback {
    media_session_id: i32,
    content_id: String,
    content_type: String,
    duration: Option<f64>,
    state: MediaState,
    idle_reason: Option<&'static str>,
    player: Option<decode::Handle>,
    session: Option<decode::Session>,
    idle_since: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaState {
    Buffering,
    Playing,
    Paused,
    Idle,
}

impl MediaState {
    fn label(self) -> &'static str {
        match self {
            Self::Buffering => "BUFFERING",
            Self::Playing => "PLAYING",
            Self::Paused => "PAUSED",
            Self::Idle => "IDLE",
        }
    }
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
            video: config.video,
            window_enabled: config.window_enabled,
            state: Mutex::new(CoreState {
                volume_level: 1.0,
                volume_muted: false,
                session: None,
                connected_senders: Vec::new(),
                media: None,
                next_media_session_id: 1,
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

    /// An idle RECEIVER_STATUS broadcast, used after idle-session teardown.
    pub fn receiver_status_broadcast(&self) -> CastMessage {
        self.receiver_status(None)
    }

    /// A debug snapshot of the active media session.
    pub fn playback_snapshot(&self) -> Option<(&'static str, f64)> {
        let state = self.state.lock().expect("receiver core state");
        let playback = state.media.as_ref()?;
        Some((
            playback.state.label(),
            playback
                .session
                .as_ref()
                .map(|session| session.clock.media_time_secs())
                .unwrap_or(0.0),
        ))
    }

    /// Whether a freshly decoded frame awaits presentation.
    pub fn has_pending_frame(&self) -> bool {
        self.video
            .as_ref()
            .map(|slot| slot.lock().expect("frame slot").is_some())
            .unwrap_or(false)
    }

    /// Stops the active media and tears the app session down.
    pub fn stop_media(&self) {
        let mut state = self.state.lock().expect("receiver core state");
        if let Some(playback) = state.media.as_mut()
            && let Some(player) = playback.player.take()
        {
            player.stop();
        }
        state.media = None;
        if state.session.is_some() {
            state.session = None;
        }
    }

    /// Toggles between playing and paused for the active media.
    pub fn toggle_pause(&self) {
        let mut state = self.state.lock().expect("receiver core state");
        let Some(playback) = state.media.as_mut() else {
            return;
        };
        let Some(player) = playback.player.as_ref() else {
            return;
        };
        let (command, next) = match playback.state {
            MediaState::Playing => (decode::Command::Pause, MediaState::Paused),
            MediaState::Paused | MediaState::Buffering => {
                (decode::Command::Play, MediaState::Playing)
            }
            MediaState::Idle => return,
        };
        player.send(command);
        playback.state = next;
    }

    /// Stops the app session once media has been idle past the timeout.
    pub fn settle_idle(&self) -> bool {
        let mut state = self.state.lock().expect("receiver core state");
        let Some(playback) = state.media.as_mut() else {
            return false;
        };
        let Some(idle_since) = playback.idle_since else {
            return false;
        };
        if idle_since.elapsed() < IDLE_TIMEOUT {
            return false;
        }
        if let Some(player) = playback.player.take() {
            player.stop();
        }
        playback.session = None;
        state.media = None;
        state.session = None;
        true
    }

    /// Emits a lifecycle event on the receiver event channel.
    pub fn notify(&self, event: ReceiverEvent) {
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

    /// Drains decode-thread events, updating playback state; returns the
    /// dispatches (broadcasts) that should go out to senders.
    pub fn poll(&self) -> Vec<server::Dispatch> {
        let mut state = self.state.lock().expect("receiver core state");
        let Some(playback) = state.media.as_mut() else {
            return Vec::new();
        };
        let mut changed = false;
        while let Some(event) = playback.player.as_ref().and_then(decode::Handle::try_event) {
            match event {
                decode::Event::Opened { duration, .. } => {
                    if playback.duration.is_none() {
                        playback.duration = duration;
                    }
                }
                decode::Event::State(state) => {
                    if !matches!(playback.state, MediaState::Idle) {
                        let next = match state {
                            decode::PlaybackState::Playing => MediaState::Playing,
                            decode::PlaybackState::Paused => MediaState::Paused,
                        };
                        if playback.state != next {
                            playback.state = next;
                        }
                    }
                }
                decode::Event::Ended => {
                    if !matches!(playback.state, MediaState::Idle) {
                        playback.state = MediaState::Idle;
                        playback.idle_reason = Some("FINISHED");
                        playback.idle_since = Some(Instant::now());
                        self.notify(ReceiverEvent::MediaEnded {
                            title: basename_of(&playback.content_id),
                        });
                        changed = true;
                    }
                }
                decode::Event::Failed(reason) => {
                    log::info!("receiver playback failed: {reason}");
                    if !matches!(playback.state, MediaState::Idle) {
                        playback.state = MediaState::Idle;
                        playback.idle_reason = Some("ERROR");
                        playback.idle_since = Some(Instant::now());
                        self.notify(ReceiverEvent::MediaFailed { detail: reason });
                        changed = true;
                    }
                }
            }
        }
        let _ = changed;
        // A status broadcast keeps senders in sync after any event drain.
        if changed {
            return vec![self.broadcast_media_status_dispatch()];
        }
        Vec::new()
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

    fn handle_heartbeat(&self, inbound: &CastMessage) -> Vec<server::Dispatch> {
        let Some(payload) = message_payload_json(inbound) else {
            return Vec::new();
        };
        match payload.get("type").and_then(Value::as_str) {
            Some("PING") => vec![server::Dispatch::Reply {
                message: json_message(
                    NS_HEARTBEAT,
                    RECEIVER_ID,
                    &inbound.source,
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
        match payload.get("type").and_then(Value::as_str) {
            Some("LOAD") => self.load_media(&sender, request_id, &payload),
            Some("GET_STATUS") => {
                vec![self.media_status_reply(&sender, request_id.unwrap_or(0))]
            }
            Some("PLAY") => self.drive_playback(&sender, request_id, decode::Command::Play),
            Some("PAUSE") => self.drive_playback(&sender, request_id, decode::Command::Pause),
            Some("SEEK") => {
                let target = payload
                    .get("currentTime")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                self.drive_playback(&sender, request_id, decode::Command::Seek(target))
            }
            Some("STOP") => self.drive_playback(&sender, request_id, decode::Command::Stop),
            Some("SET_VOLUME") => self.set_media_volume(&sender, request_id, &payload),
            _ => vec![reply_to(
                NS_MEDIA,
                RECEIVER_ID,
                &sender,
                json!({"type": "INVALID_REQUEST", "requestId": request_id.unwrap_or(0)}),
            )],
        }
    }

    /// Starts local playback for a sender-supplied URL.
    fn load_media(
        &self,
        sender: &str,
        request_id: Option<u64>,
        payload: &Value,
    ) -> Vec<server::Dispatch> {
        let request_id = request_id.unwrap_or(0);
        let Some(media) = payload.get("media") else {
            return vec![load_failed(sender, request_id)];
        };
        let content_id = media
            .get("contentId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let content_type = media
            .get("contentType")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_owned();
        let stream_type = media
            .get("streamType")
            .and_then(Value::as_str)
            .unwrap_or("BUFFERED");
        let title = media
            .get("metadata")
            .and_then(|metadata| metadata.get("title"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| basename_of(&content_id));
        let start_at = payload
            .get("currentTime")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let autoplay = payload
            .get("autoplay")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        if let Err(error) = fetch::classify_load(&content_id, &content_type, stream_type) {
            log::info!("rejecting LOAD: {error}");
            self.notify(ReceiverEvent::LoadRejected {
                sender: sender.to_owned(),
            });
            return vec![load_failed(sender, request_id)];
        }

        let video_slot = self.video.as_ref().map(Arc::clone);

        let mut state = self.state.lock().expect("receiver core state");
        // Replace any current playback: the Default Media Receiver interrupts
        // it with a new LOAD.
        if let Some(previous) = state.media.as_mut()
            && let Some(player) = previous.player.take()
        {
            player.stop();
        }
        let media_session_id = state.next_media_session_id;
        state.next_media_session_id += 1;
        let (player, session) = decode::spawn(
            decode::Source::Origin(content_id.clone()),
            title,
            start_at,
            autoplay,
            self.window_enabled,
            video_slot.unwrap_or_else(|| Arc::new(Mutex::new(None))),
        );
        state.media = Some(MediaPlayback {
            media_session_id,
            content_id: content_id.clone(),
            content_type: content_type.clone(),
            duration: None,
            state: MediaState::Buffering,
            idle_reason: None,
            player: Some(player),
            session: Some(session),
            idle_since: None,
        });
        drop(state);
        self.notify(ReceiverEvent::MediaLoading {
            title: basename_of(&content_id),
        });
        vec![
            self.media_status_reply(sender, request_id),
            self.broadcast_media_status_dispatch(),
        ]
    }

    /// Forwards a transport command to the active player and echoes the
    /// resulting media status.
    fn drive_playback(
        &self,
        sender: &str,
        request_id: Option<u64>,
        command: decode::Command,
    ) -> Vec<server::Dispatch> {
        let mut broadcast = false;
        {
            let mut state = self.state.lock().expect("receiver core state");
            let Some(playback) = state.media.as_mut() else {
                drop(state);
                return vec![self.media_status_reply(sender, request_id.unwrap_or(0))];
            };
            if let Some(player) = playback.player.as_ref() {
                player.send(command.clone());
            }
            match (playback.state, &command) {
                (MediaState::Idle, _) => {}
                (_, decode::Command::Play) => {
                    if playback.state != MediaState::Playing {
                        playback.state = MediaState::Playing;
                        playback.idle_reason = None;
                        broadcast = true;
                    }
                }
                (_, decode::Command::Pause) => {
                    if playback.state != MediaState::Paused {
                        playback.state = MediaState::Paused;
                        broadcast = true;
                    }
                }
                (_, decode::Command::Stop) => {
                    playback.state = MediaState::Idle;
                    playback.idle_reason = Some("CANCELLED");
                    playback.idle_since = Some(Instant::now());
                    broadcast = true;
                }
                (_, decode::Command::Seek(_)) => {}
                (_, decode::Command::SetVolume { .. }) => {}
            }
        }
        let mut dispatches = vec![self.media_status_reply(sender, request_id.unwrap_or(0))];
        if broadcast {
            dispatches.push(self.broadcast_media_status_dispatch());
        }
        dispatches
    }

    /// Maps a session-level SET_VOLUME to the player's output gain.
    fn set_media_volume(
        &self,
        sender: &str,
        request_id: Option<u64>,
        payload: &Value,
    ) -> Vec<server::Dispatch> {
        if let Some(volume) = payload.get("volume") {
            let level = volume.get("level").and_then(Value::as_f64);
            let muted = volume.get("muted").and_then(Value::as_bool);
            let (fallback_level, fallback_muted) = {
                let state = self.state.lock().expect("receiver core state");
                (state.volume_level, state.volume_muted)
            };
            let mut state = self.state.lock().expect("receiver core state");
            if let Some(playback) = state.media.as_mut()
                && let Some(session) = playback.session.as_ref()
            {
                let next_level = level.unwrap_or(fallback_level);
                let next_muted = muted.unwrap_or(fallback_muted);
                session.volume.set(next_level, next_muted);
            }
        }
        vec![self.media_status_reply(sender, request_id.unwrap_or(0))]
    }

    /// A MEDIA_STATUS addressed back to the requesting sender.
    fn media_status_reply(&self, sender: &str, request_id: u64) -> server::Dispatch {
        reply_to(
            NS_MEDIA,
            RECEIVER_ID,
            sender,
            self.media_status_payload(request_id),
        )
    }

    fn broadcast_media_status_dispatch(&self) -> server::Dispatch {
        server::Dispatch::Broadcast {
            message: json_message(NS_MEDIA, RECEIVER_ID, "*", self.media_status_payload(0)),
        }
    }

    fn media_status_payload(&self, request_id: u64) -> Value {
        let state = self.state.lock().expect("receiver core state");
        let entries = match &state.media {
            Some(playback) => {
                let current_time = playback
                    .session
                    .as_ref()
                    .map(|session| session.clock.media_time_secs())
                    .unwrap_or(0.0);
                vec![json!({
                    "mediaSessionId": playback.media_session_id,
                    "media": {
                        "contentId": playback.content_id,
                        "streamType": "BUFFERED",
                        "contentType": playback.content_type,
                        "duration": playback.duration,
                    },
                    "playbackRate": 1.0,
                    "playerState": playback.state.label(),
                    "idleReason": playback.idle_reason,
                    "currentTime": current_time,
                    "supportedMediaCommands": SUPPORTED_MEDIA_COMMANDS,
                })]
            }
            None => Vec::new(),
        };
        json!({
            "type": "MEDIA_STATUS",
            "requestId": request_id,
            "status": entries,
        })
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
        // Broadcasts carry requestId 0; rust_cast's parser requires the field.
        payload.insert("requestId".into(), Value::from(request_id.unwrap_or(0)));
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
            "isStandBy": state.session.is_none(),
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
                // Active playback ends together with the app session.
                if let Some(playback) = state.media.as_mut()
                    && let Some(player) = playback.player.take()
                {
                    player.stop();
                }
                state.media = None;
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

fn load_failed(sender: &str, request_id: u64) -> server::Dispatch {
    reply_to(
        NS_MEDIA,
        RECEIVER_ID,
        sender,
        json!({"type": "LOAD_FAILED", "requestId": request_id}),
    )
}

fn basename_of(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    StdPath::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "media".to_owned())
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
            video: None,
            window_enabled: false,
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
        assert_eq!(payload["status"]["isStandBy"], true);
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
        assert_eq!(reply["status"]["isStandBy"], false);
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
    fn loads_of_unsupported_sources_are_rejected() {
        let (core, events) = core();
        let dispatches = core.handle_inbound(
            1,
            &inbound(NS_MEDIA, r#"{"type":"LOAD","requestId":11,"media":{"contentId":"http://example.invalid/movie.m3u8","contentType":"application/x-mpegURL"}}"#),
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
    fn loads_of_supported_sources_start_buffering() {
        let (core, _) = core();
        let dispatches = core.handle_inbound(
            1,
            &inbound(
                NS_MEDIA,
                r#"{"type":"LOAD","requestId":12,"autoplay":true,"currentTime":2.5,"media":{"contentId":"http://example.invalid/movie.mp4","contentType":"video/mp4"}}"#,
            ),
        );
        let payload = payload_of(&dispatches[0]);
        assert_eq!(payload["type"], "MEDIA_STATUS");
        let _entry = &payload["status"][0];
        assert_eq!(payload["requestId"], 12);
        assert!(entry_media_session_id(&payload) >= 1);
        assert_eq!(entry_state(&payload), "BUFFERING");
        assert_eq!(payload["status"][0]["media"]["contentType"], "video/mp4");
        // The decode thread fails fast on an unreachable origin; statuses
        // still echo the load until the failure event lands.
        let _ = session_media_session_id(&core);
    }

    fn entry_media_session_id(payload: &Value) -> i64 {
        payload["status"][0]["mediaSessionId"].as_i64().unwrap()
    }

    fn entry_state(payload: &Value) -> String {
        payload["status"][0]["playerState"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn session_media_session_id(core: &ReceiverCore) -> i64 {
        let dispatches = core.handle_inbound(1, &inbound(NS_MEDIA, r#"{"type":"GET_STATUS"}"#));
        entry_media_session_id(&payload_of(&dispatches[0]))
    }

    #[test]
    fn play_and_pause_commands_drive_the_reported_state() {
        let (core, _) = core();
        core.handle_inbound(
            1,
            &inbound(
                NS_MEDIA,
                r#"{"type":"LOAD","autoplay":false,"media":{"contentId":"http://example.invalid/movie.mp4","contentType":"video/mp4"}}"#,
            ),
        );
        core.poll();
        core.handle_inbound(1, &inbound(NS_MEDIA, r#"{"type":"PLAY","requestId":2}"#));
        core.poll();
        let dispatches = core.handle_inbound(1, &inbound(NS_MEDIA, r#"{"type":"GET_STATUS"}"#));
        // The decode thread may have failed already (origin unreachable in
        // tests), which flips the state to IDLE; both the BUFFERING and IDLE
        // outcomes are acceptable here.
        let state = entry_state(&payload_of(&dispatches[0]));
        assert!(state == "BUFFERING" || state == "IDLE" || state == "PLAYING");
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
        assert_eq!(payload["status"], json!([]));
    }

    #[test]
    fn unknown_media_commands_are_invalid_requests() {
        let (core, _) = core();
        let dispatches = core.handle_inbound(
            1,
            &inbound(NS_MEDIA, r#"{"type":"QUEUE_LOAD","requestId":1}"#),
        );
        assert_eq!(payload_of(&dispatches[0])["type"], "INVALID_REQUEST");
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

    #[test]
    fn base_names_survive_query_strings() {
        assert_eq!(basename_of("http://a/b/c.mp4?token=1#frag"), "c.mp4");
        assert_eq!(basename_of("not-a-url"), "not-a-url");
    }
}
