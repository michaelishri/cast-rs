use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use protobuf::Message;
use rust_cast::cast::cast_channel as raw;
use rust_cast::cast::cast_channel::cast_message::PayloadType;
use rust_cast::message_manager::{CastMessage, CastMessagePayload};
use rustls::pki_types::CertificateDer;
use rustls::{ServerConfig, ServerConnection};

use super::ReceiverEvent;
use super::auth::Identity;
use super::platform::{NS_DEVICEAUTH, NS_HEARTBEAT, RECEIVER_ID, ReceiverCore};

const MAX_MESSAGE_BYTES: u32 = 1024 * 1024;
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(200);
const CONNECTION_READ_TIMEOUT: Duration = Duration::from_millis(100);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
const JANITOR_INTERVAL: Duration = Duration::from_millis(500);

/// What the core wants the server to do after handling an inbound message.
#[derive(Debug, Clone, PartialEq)]
pub enum Dispatch {
    /// Send a message back to the connection that triggered it.
    Reply { message: CastMessage },
    /// Send a message to every connected sender.
    Broadcast { message: CastMessage },
}

pub struct Server {
    shutdown: Arc<AtomicBool>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl Server {
    /// Starts accepting TLS connections and spawns the connection loops plus
    /// the heartbeat janitor.
    pub fn start(
        listener: TcpListener,
        core: Arc<ReceiverCore>,
        identity: Arc<Identity>,
        accept: HashSet<IpAddr>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<Self> {
        let config = Arc::new(build_tls_config(&identity)?);
        let registry = Arc::new(ConnectionRegistry::new());

        listener
            .set_nonblocking(true)
            .context("could not make the receiver listener non-blocking")?;

        let mut workers = Vec::new();
        {
            let worker_listener = listener
                .try_clone()
                .context("could not clone the listener for the accept worker")?;
            let registry = Arc::clone(&registry);
            let core = Arc::clone(&core);
            let shutdown = Arc::clone(&shutdown);
            workers.push(
                std::thread::Builder::new()
                    .name("cast-receiver-accept".to_owned())
                    .spawn(move || {
                        accept_loop(
                            worker_listener,
                            config,
                            registry,
                            core,
                            identity,
                            accept,
                            shutdown,
                        );
                    })
                    .context("could not start the receiver accept worker")?,
            );
        }
        {
            let registry = Arc::clone(&registry);
            let core = Arc::clone(&core);
            let shutdown = Arc::clone(&shutdown);
            workers.push(
                std::thread::Builder::new()
                    .name("cast-receiver-janitor".to_owned())
                    .spawn(move || {
                        housekeeper_loop(registry, core, shutdown);
                    })
                    .context("could not start the receiver heartbeat janitor")?,
            );
        }

        Ok(Self {
            shutdown,
            workers: Mutex::new(workers),
        })
    }

    /// Signals every worker to stop.
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let mut workers = self.workers.lock().expect("worker list");
        for worker in workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn build_tls_config(identity: &Identity) -> Result<ServerConfig> {
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(identity.certificate.clone())],
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                identity.private_key.clone(),
            )),
        )
        .context("could not build the receiver TLS configuration")
}

/// Registry of live sender connections, shared by the accept loop, connection
/// threads, and the heartbeat janitor.
struct ConnectionRegistry {
    connections: Mutex<HashMap<u64, ConnectionEntry>>,
    next_id: AtomicU64,
}

struct ConnectionEntry {
    outbound: Sender<CastMessage>,
    stop: Arc<AtomicBool>,
    last_seen: Arc<Mutex<Instant>>,
    last_ping: Arc<Mutex<Instant>>,
    peer: String,
}

impl ConnectionRegistry {
    fn new() -> Self {
        Self {
            connections: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn register(&self, entry: ConnectionEntry) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.connections
            .lock()
            .expect("connection registry")
            .insert(id, entry);
        id
    }

    fn send(&self, conn: u64, message: CastMessage) {
        let connections = self.connections.lock().expect("connection registry");
        if let Some(entry) = connections.get(&conn) {
            entry.outbound.send(message).ok();
        }
    }

    fn broadcast(&self, message: &CastMessage) {
        let connections = self.connections.lock().expect("connection registry");
        for entry in connections.values() {
            entry.outbound.send(message.clone()).ok();
        }
    }

    fn broadcast_dispatches(&self, dispatches: &[Dispatch]) {
        for dispatch in dispatches {
            if let Dispatch::Broadcast { message } = dispatch {
                self.broadcast(message);
            }
        }
    }

    fn stop(&self, conn: u64) {
        let connections = self.connections.lock().expect("connection registry");
        if let Some(entry) = connections.get(&conn) {
            entry.stop.store(true, Ordering::SeqCst);
        }
    }

    fn remove(&self, conn: u64) -> Option<String> {
        let removed = self
            .connections
            .lock()
            .expect("connection registry")
            .remove(&conn);
        removed.map(|entry| {
            entry.stop.store(true, Ordering::SeqCst);
            entry.peer
        })
    }

    fn apply(&self, conn: u64, dispatches: Vec<Dispatch>) {
        for dispatch in dispatches {
            match dispatch {
                Dispatch::Reply { message } => self.send(conn, message),
                Dispatch::Broadcast { message } => self.broadcast(&message),
            }
        }
    }
}

fn accept_loop(
    listener: TcpListener,
    config: Arc<ServerConfig>,
    registry: Arc<ConnectionRegistry>,
    core: Arc<ReceiverCore>,
    identity: Arc<Identity>,
    accept: HashSet<IpAddr>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        let (stream, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
                continue;
            }
            Err(error) => {
                log::debug!("receiver accept failed: {error}");
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
                continue;
            }
        };
        if !accept.is_empty() && !accept.contains(&peer.ip()) {
            log::info!("rejected sender {peer}: not on the allowlist");
            continue;
        }
        log::info!("sender connected: {peer}");
        core.notify(ReceiverEvent::SenderConnected {
            peer: peer.to_string(),
        });
        spawn_connection(
            registry.clone(),
            core.clone(),
            config.clone(),
            Arc::clone(&identity),
            stream,
            peer,
        );
    }
}

/// Everything one connection loop needs from its registry entry.
struct ConnectionContext {
    core: Arc<ReceiverCore>,
    identity: Arc<Identity>,
    conn: u64,
    last_seen: Arc<Mutex<Instant>>,
    outbound: Receiver<CastMessage>,
    stop: Arc<AtomicBool>,
}

fn spawn_connection(
    registry: Arc<ConnectionRegistry>,
    core: Arc<ReceiverCore>,
    config: Arc<ServerConfig>,
    identity: Arc<Identity>,
    stream: TcpStream,
    peer: SocketAddr,
) {
    let (outbound_tx, outbound_rx) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let last_seen = Arc::new(Mutex::new(Instant::now()));
    let last_ping = Arc::new(Mutex::new(Instant::now()));
    let entry = ConnectionEntry {
        outbound: outbound_tx,
        stop: Arc::clone(&stop),
        last_seen: Arc::clone(&last_seen),
        last_ping: Arc::clone(&last_ping),
        peer: peer.to_string(),
    };
    let conn = registry.register(entry);
    let context = ConnectionContext {
        core,
        identity,
        conn,
        last_seen,
        outbound: outbound_rx,
        stop,
    };
    let loop_registry = Arc::clone(&registry);
    let handle = std::thread::Builder::new()
        .name(format!("cast-receiver-conn-{conn}"))
        .spawn(move || {
            connection_loop(loop_registry, context, config, stream);
        });
    if handle.is_err() {
        registry.remove(conn);
    }
}

fn connection_loop(
    registry: Arc<ConnectionRegistry>,
    mut context: ConnectionContext,
    config: Arc<ServerConfig>,
    mut stream: TcpStream,
) {
    // Accepted sockets inherit the listener's non-blocking mode on BSD- and
    // macOS-derived kernels; the connection loop needs blocking reads with a
    // timeout so rustls can drive the handshake and frame reads in order.
    stream
        .set_nonblocking(false)
        .and_then(|()| stream.set_read_timeout(Some(CONNECTION_READ_TIMEOUT)))
        .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(10))))
        .ok();
    let mut tls = match ServerConnection::new(config) {
        Ok(tls) => tls,
        Err(error) => {
            log::info!(
                "could not start a TLS session with connection {}: {error}",
                context.conn
            );
            return;
        }
    };
    let outcome = run_connection(&registry, &mut context, &mut tls, &mut stream);
    if let Err(error) = outcome {
        log::debug!("sender connection ended: {error}");
    }
    // Announce the shutdown to the sender before closing, best effort.
    tls.send_close_notify();
    let _ = flush_tls(&mut tls, &mut stream);
    let peer = registry.remove(context.conn);
    if let Some(peer) = peer {
        log::info!("sender disconnected: {peer}");
        context
            .core
            .notify(ReceiverEvent::SenderDisconnected { peer });
    }
}

fn run_connection(
    registry: &Arc<ConnectionRegistry>,
    context: &mut ConnectionContext,
    tls: &mut ServerConnection,
    stream: &mut TcpStream,
) -> Result<()> {
    let conn = context.conn;
    let mut frames = FrameParser::default();
    loop {
        if context.stop.load(Ordering::SeqCst) {
            return Ok(());
        }

        // Queue everything addressed to this sender: replies, broadcasts,
        // and janitor pings.
        while let Ok(message) = context.outbound.try_recv() {
            if let Err(error) = enqueue_frame(tls, &message) {
                return Err(error.context("queuing an outbound frame"));
            }
        }
        if let Err(error) = flush_tls(tls, stream) {
            return Err(error.context("flushing the TLS queue"));
        }

        match tls.read_tls(stream) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                if let Err(error) = tls.process_new_packets() {
                    return Err(anyhow::Error::new(error).context("processing TLS packets"));
                }
                *context.last_seen.lock().expect("last seen") = Instant::now();
                if let Err(error) = flush_tls(tls, stream) {
                    return Err(error.context("flushing the TLS handshake"));
                }
                let mut plaintext = Vec::new();
                // rustls's Reader returns Err(WouldBlock) when no plaintext
                // is pending; that simply means nothing to extract yet.
                let mut chunk = [0_u8; 8192];
                loop {
                    match tls.reader().read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => plaintext.extend_from_slice(&chunk[..read]),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) => {
                            return Err(anyhow::Error::new(error).context("reading TLS plaintext"));
                        }
                    }
                }
                for frame in frames.feed(&plaintext) {
                    let message = match parse_frame(&frame) {
                        Ok(message) => message,
                        Err(error) => {
                            log::debug!("dropping malformed frame from {conn}: {error}");
                            continue;
                        }
                    };
                    // Device auth is answered directly with the receiver's
                    // own certificate chain instead of through the core.
                    if message.namespace == NS_DEVICEAUTH {
                        let CastMessagePayload::Binary(challenge) = &message.payload else {
                            continue;
                        };
                        if let Some(response) = context.identity.respond_to_challenge(challenge) {
                            registry.send(
                                conn,
                                CastMessage {
                                    namespace: NS_DEVICEAUTH.to_owned(),
                                    source: RECEIVER_ID.to_owned(),
                                    destination: message.source.clone(),
                                    payload: CastMessagePayload::Binary(response),
                                },
                            );
                        }
                        continue;
                    }
                    registry.apply(conn, context.core.handle_inbound(conn, &message));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(anyhow::Error::new(error).context("socket read failed")),
        }
    }
}

/// The housekeeper drives heartbeats, player-event polling, and idle-session
/// teardown in one background loop.
fn housekeeper_loop(
    registry: Arc<ConnectionRegistry>,
    core: Arc<ReceiverCore>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(JANITOR_INTERVAL);
        // Apply any playback state changes as status broadcasts.
        let dispatches = core.poll();
        if !dispatches.is_empty() {
            registry.broadcast_dispatches(&dispatches);
        }
        if core.settle_idle() {
            registry.broadcast(&core.receiver_status_broadcast());
        }
        if let Some((state, position)) = core.playback_snapshot() {
            log::trace!("receiver playback: state={state} position={position:.2}s");
        }
        janitor_sweep(&registry);
    }
}

fn janitor_sweep(registry: &Arc<ConnectionRegistry>) {
    let now = Instant::now();
    let silent: Vec<u64> = {
        let connections = registry.connections.lock().expect("connection registry");
        connections
            .iter()
            .filter_map(|(conn, entry)| {
                let last_seen = *entry.last_seen.lock().expect("last seen");
                if now.saturating_duration_since(last_seen) > HEARTBEAT_TIMEOUT {
                    return Some(*conn);
                }
                let mut last_ping = entry.last_ping.lock().expect("last ping");
                let last_ping_value = *last_ping;
                if now.saturating_duration_since(last_ping_value) >= HEARTBEAT_INTERVAL {
                    *last_ping = now;
                    let ping = CastMessage {
                        namespace: NS_HEARTBEAT.to_owned(),
                        source: RECEIVER_ID.to_owned(),
                        destination: "*".to_owned(),
                        payload: CastMessagePayload::String(r#"{"type":"PING"}"#.to_owned()),
                    };
                    if entry.outbound.send(ping).is_err() {
                        return Some(*conn);
                    }
                }
                None
            })
            .collect()
    };
    for conn in silent {
        log::debug!("closing silent sender connection {conn}");
        registry.stop(conn);
    }
}

#[derive(Default)]
struct FrameParser {
    buffer: Vec<u8>,
}

impl FrameParser {
    /// Accumulates decrypted bytes and returns every complete
    /// length-prefixed message payload seen so far.
    fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(data);
        let mut frames = Vec::new();
        loop {
            if self.buffer.len() < 4 {
                break;
            }
            let length = u32::from_be_bytes([
                self.buffer[0],
                self.buffer[1],
                self.buffer[2],
                self.buffer[3],
            ]);
            if length > MAX_MESSAGE_BYTES {
                log::warn!("oversized Cast message ({length} bytes); discarding the buffer");
                self.buffer.clear();
                break;
            }
            let length = length as usize;
            if self.buffer.len() < 4 + length {
                break;
            }
            frames.push(self.buffer.drain(..4 + length).skip(4).collect());
        }
        frames
    }
}

fn parse_frame(frame: &[u8]) -> Result<CastMessage> {
    let raw = raw::CastMessage::parse_from_bytes(frame)
        .map_err(|error| anyhow::anyhow!("could not parse CastMessage: {error}"))?;
    let payload = match raw
        .payload_type
        .unwrap_or_default()
        .enum_value_or(PayloadType::STRING)
    {
        PayloadType::STRING => CastMessagePayload::String(raw.payload_utf8().to_owned()),
        PayloadType::BINARY => CastMessagePayload::Binary(raw.payload_binary().to_vec()),
    };
    Ok(CastMessage {
        namespace: raw.namespace().to_owned(),
        source: raw.source_id().to_owned(),
        destination: raw.destination_id().to_owned(),
        payload,
    })
}

/// Queues one Cast message as a length-prefixed frame in the TLS writer.
fn enqueue_frame(tls: &mut ServerConnection, message: &CastMessage) -> Result<()> {
    let payload = encode_payload(message)?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    tls.writer()
        .write_all(&frame)
        .context("could not queue a Cast frame")
}

/// Drives pending TLS writes to the socket.
fn flush_tls(tls: &mut ServerConnection, stream: &mut TcpStream) -> Result<()> {
    while tls.wants_write() {
        tls.write_tls(stream)?;
    }
    stream.flush().context("could not flush the socket")
}

fn encode_payload(message: &CastMessage) -> Result<Vec<u8>> {
    let mut raw = raw::CastMessage::new();
    raw.set_protocol_version(raw::cast_message::ProtocolVersion::CASTV2_1_0);
    raw.set_namespace(message.namespace.clone());
    raw.set_source_id(message.source.clone());
    raw.set_destination_id(message.destination.clone());
    match &message.payload {
        CastMessagePayload::String(payload) => {
            raw.set_payload_type(PayloadType::STRING);
            raw.set_payload_utf8(payload.clone());
        }
        CastMessagePayload::Binary(payload) => {
            raw.set_payload_type(PayloadType::BINARY);
            raw.set_payload_binary(payload.clone());
        }
    }
    raw.write_to_bytes()
        .context("could not encode a CastMessage")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_bytes(namespace: &str, source: &str, destination: &str, payload: &str) -> Vec<u8> {
        let mut raw = raw::CastMessage::new();
        raw.set_protocol_version(raw::cast_message::ProtocolVersion::CASTV2_1_0);
        raw.set_namespace(namespace.to_owned());
        raw.set_source_id(source.to_owned());
        raw.set_destination_id(destination.to_owned());
        raw.set_payload_type(PayloadType::STRING);
        raw.set_payload_utf8(payload.to_owned());
        let bytes = raw.write_to_bytes().unwrap();
        let mut frame = Vec::with_capacity(4 + bytes.len());
        frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        frame.extend_from_slice(&bytes);
        frame
    }

    #[test]
    fn payloads_round_trip_through_the_codec() {
        let message = CastMessage {
            namespace: "urn:x-cast:com.google.cast.receiver".to_owned(),
            source: "sender-0".to_owned(),
            destination: "receiver-0".to_owned(),
            payload: CastMessagePayload::String(r#"{"type":"PING"}"#.to_owned()),
        };
        let payload = encode_payload(&message).unwrap();
        let parsed = parse_frame(&payload).unwrap();
        assert_eq!(parsed.namespace, message.namespace);
        assert_eq!(parsed.source, message.source);
        assert_eq!(parsed.destination, message.destination);
        assert_eq!(parsed.payload, message.payload);
    }

    #[test]
    fn rust_cast_encoded_frames_parse_the_same() {
        // rust_cast's own sender encoding must be decodable by the receiver.
        let frame = frame_bytes(
            "urn:x-cast:com.google.cast.tp.heartbeat",
            "sender-0",
            "receiver-0",
            r#"{"type":"PING"}"#,
        );
        let (length_bytes, payload) = frame.split_at(4);
        let length = u32::from_be_bytes([
            length_bytes[0],
            length_bytes[1],
            length_bytes[2],
            length_bytes[3],
        ]);
        assert_eq!(length as usize, payload.len());
        let parsed = parse_frame(payload).unwrap();
        assert_eq!(parsed.namespace, "urn:x-cast:com.google.cast.tp.heartbeat");
        assert_eq!(parsed.source, "sender-0");
        assert_eq!(parsed.destination, "receiver-0");
        assert_eq!(
            parsed.payload,
            CastMessagePayload::String(r#"{"type":"PING"}"#.to_owned())
        );
    }

    #[test]
    fn the_frame_parser_handles_fragmented_and_combined_reads() {
        let frame = frame_bytes(
            "urn:x-cast:com.google.cast.receiver",
            "sender-0",
            "receiver-0",
            r#"{"type":"GET_STATUS"}"#,
        );
        let mut parser = FrameParser::default();
        assert!(parser.feed(&frame[..3]).is_empty());
        assert!(parser.feed(&frame[3..10]).is_empty());
        let rest = parser.feed(&frame[10..]);
        assert_eq!(rest.len(), 1);

        let mut parser = FrameParser::default();
        let mut combined = frame.clone();
        combined.extend_from_slice(&frame);
        let frames = parser.feed(&combined);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], frames[1]);
    }

    #[test]
    fn oversized_frames_discard_the_buffer() {
        let mut parser = FrameParser::default();
        let oversized = (MAX_MESSAGE_BYTES + 1).to_be_bytes();
        assert!(parser.feed(&oversized).is_empty());
        // The buffer was cleared, so a following frame still parses.
        let frame = frame_bytes(
            "urn:x-cast:com.google.cast.tp.heartbeat",
            "sender-0",
            "receiver-0",
            r#"{"type":"PING"}"#,
        );
        assert_eq!(parser.feed(&frame).len(), 1);
    }

    #[test]
    fn binary_payloads_round_trip() {
        let message = CastMessage {
            namespace: "urn:x-cast:com.google.cast.tp.deviceauth".to_owned(),
            source: "sender-0".to_owned(),
            destination: "receiver-0".to_owned(),
            payload: CastMessagePayload::Binary(vec![1, 2, 3, 4]),
        };
        let payload = encode_payload(&message).unwrap();
        let parsed = parse_frame(&payload).unwrap();
        assert_eq!(parsed.payload, CastMessagePayload::Binary(vec![1, 2, 3, 4]));
    }
}
