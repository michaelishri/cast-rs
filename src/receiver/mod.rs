pub mod advertise;
pub mod auth;
pub mod clock;
pub mod decode;
pub mod fetch;
pub mod platform;
pub mod server;
pub mod window;

use std::collections::HashSet;
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use crate::discovery::DeviceCapability;

pub use platform::ReceiverCore;

const INTERRUPT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Whether the receiver should advertise video output, audio output only, or
/// detect what the machine supports at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilityPreference {
    Auto,
    Video,
    AudioOnly,
}

#[derive(Clone, Debug)]
pub struct ReceiveOptions {
    /// Advertised friendly name; defaults to `"<hostname> Cast"`.
    pub name: Option<String>,
    /// Advertised model name.
    pub model: String,
    /// Cast protocol port.
    pub port: u16,
    /// Advertised capability; auto prefers video with a display attached.
    pub capabilities: CapabilityPreference,
    /// Interface to bind; defaults to all interfaces.
    pub bind: Option<IpAddr>,
    /// Sender allowlist; empty accepts every sender on the LAN.
    pub accept: Vec<IpAddr>,
    /// Never open a video window (honoured fully once media playback lands).
    #[allow(dead_code)]
    pub no_window: bool,
    /// Print machine-readable one-line JSON status events.
    pub json: bool,
    /// Exit after this many seconds; otherwise run until interrupted.
    pub seconds: Option<u64>,
}

/// Lifecycle events emitted while the receiver runs.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum ReceiverEvent {
    Listening {
        name: String,
        model: String,
        port: u16,
        capability: DeviceCapability,
    },
    SenderConnected {
        peer: String,
    },
    SenderDisconnected {
        peer: String,
    },
    Launched {
        app_id: String,
        sender: String,
    },
    SessionStopped {
        sender: String,
    },
    VolumeChanged {
        level: f64,
        muted: bool,
    },
    LoadRejected {
        sender: String,
    },
    VideoWindow,
    MediaLoading {
        title: String,
    },
    MediaEnded {
        title: String,
    },
    MediaFailed {
        detail: String,
    },
    Shutdown,
}

pub fn run(options: ReceiveOptions) -> Result<()> {
    let friendly_name = options.name.clone().unwrap_or_else(default_name);
    if friendly_name.trim().is_empty() {
        return Err(anyhow!("the receiver name must not be empty"));
    }
    let model = if options.model.trim().is_empty() {
        "Cast Desktop Receiver".to_owned()
    } else {
        options.model.trim().to_owned()
    };
    let capability = match options.capabilities {
        CapabilityPreference::Auto => auto_capability(),
        CapabilityPreference::Video => DeviceCapability::Video,
        CapabilityPreference::AudioOnly => DeviceCapability::AudioOnly,
    };
    let receiver_id = persisted_receiver_id()?;
    let identity = auth::Identity::generate(&friendly_name)?;

    let bind_addr = SocketAddr::new(
        options
            .bind
            .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
        options.port,
    );
    let listener = TcpListener::bind(bind_addr).with_context(|| {
        format!(
            "could not bind the Cast protocol listener on {bind_addr}; \
             pass --port to pick another port"
        )
    })?;
    let port = listener.local_addr()?.port();

    let advertisement = advertise::AdvertisedService::start(advertise::AdvertiseOptions {
        receiver_id,
        friendly_name: friendly_name.clone(),
        model: model.clone(),
        capability,
        port,
    })?;

    let (events, event_rx) = std::sync::mpsc::channel();
    let mut event_rx = Some(event_rx);
    let window_enabled = capability == DeviceCapability::Video && !options.no_window;
    let video_slot: decode::FrameSlot = Arc::new(Mutex::new(None));

    // The winit event loop must own the main thread. Without a reachable
    // window system the receiver falls back to audio-only playback.
    let event_loop = if window_enabled {
        match winit::event_loop::EventLoop::<window::FrameReady>::with_user_event().build() {
            Ok(event_loop) => Some(event_loop),
            Err(error) => {
                log::info!(
                    "no window system is reachable ({error}); running without a video window"
                );
                None
            }
        }
    } else {
        None
    };

    let core = Arc::new(ReceiverCore::new(platform::CoreConfig {
        name: friendly_name.clone(),
        model: model.clone(),
        capability,
        events: events.clone(),
        video: Some(Arc::clone(&video_slot)),
        window_enabled,
    }));
    let shutdown = Arc::new(AtomicBool::new(false));
    let interrupt = install_interrupt_handler(Arc::clone(&shutdown))?;

    let server = server::Server::start(
        listener,
        Arc::clone(&core),
        Arc::new(identity),
        HashSet::from_iter(options.accept.iter().copied()),
        Arc::clone(&shutdown),
    )?;

    events
        .send(ReceiverEvent::Listening {
            name: core.name().to_owned(),
            model: core.model().to_owned(),
            port,
            capability: core.capability(),
        })
        .context("receiver event channel closed")?;

    let deadline = options
        .seconds
        .map(|seconds| Instant::now() + Duration::from_secs(seconds));
    if let Some(event_loop) = event_loop {
        let _ = events.send(ReceiverEvent::VideoWindow);
        *event_rx
            .as_mut()
            .expect("events are handed over exactly once") = window::run(
            window::WindowConfig {
                title: friendly_name,
                slot: video_slot,
                core: Arc::clone(&core),
                events: event_rx
                    .take()
                    .expect("events are handed to the window once"),
                json: options.json,
                stop: Arc::clone(&shutdown),
                deadline,
            },
            event_loop,
        );
    } else {
        let event_rx = event_rx
            .take()
            .expect("events are handed to the poll loop once");
        loop {
            if shutdown.load(Ordering::SeqCst) || interrupt.load(Ordering::SeqCst) {
                break;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                break;
            }
            drain_events(&event_rx, options.json, false);
            std::thread::sleep(INTERRUPT_POLL_INTERVAL);
        }
    }

    server.stop();
    drop(server);
    advertisement.stop();
    let _ = events.send(ReceiverEvent::Shutdown);
    if let Some(event_rx) = event_rx {
        drain_events(&event_rx, options.json, true);
    }
    Ok(())
}

/// Prints one receiver event, either as JSON on stdout or as a log line.
pub fn emit_event(event: &ReceiverEvent, json: bool) {
    if json {
        let mut line = serde_json::to_string(event).expect("events serialize to JSON");
        line.push('\n');
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(line.as_bytes()).ok();
        stdout.flush().ok();
    } else {
        log::info!("{event:?}");
    }
}

fn drain_events(event_rx: &std::sync::mpsc::Receiver<ReceiverEvent>, json: bool, blocking: bool) {
    loop {
        let event = if blocking {
            event_rx.recv_timeout(Duration::from_millis(50)).ok()
        } else {
            event_rx.try_recv().ok()
        };
        let Some(event) = event else { return };
        if json {
            let mut line = serde_json::to_string(&event).expect("events serialize to JSON");
            line.push('\n');
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(line.as_bytes()).ok();
            stdout.flush().ok();
        } else {
            log::info!("{event:?}");
        }
    }
}

fn install_interrupt_handler(shutdown: Arc<AtomicBool>) -> Result<Arc<AtomicBool>> {
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&interrupted);
    let shutdown_signal = Arc::clone(&shutdown);
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::SeqCst);
        // The windowed event loop only watches the shutdown flag; both sides
        // observe a single Ctrl-C.
        shutdown_signal.store(true, Ordering::SeqCst);
    })
    .context("could not install Ctrl-C handler")?;
    Ok(interrupted)
}

fn default_name() -> String {
    format!(
        "{} Cast",
        hostname().unwrap_or_else(|| "cast-receiver".to_owned())
    )
}

fn hostname() -> Option<String> {
    let mut buffer = [0_u8; 256];
    // SAFETY: the buffer is valid for the duration of the call and gethostname
    // NUL-terminates within its length on supported platforms.
    let result =
        unsafe { libc::gethostname(buffer.as_mut_ptr() as *mut libc::c_char, buffer.len()) };
    if result != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    let hostname = std::str::from_utf8(&buffer[..end]).ok()?;
    let hostname = hostname.trim();
    (!hostname.is_empty()).then(|| hostname.to_owned())
}

/// Returns the stable receiver UUID used in mDNS advertisements, creating it
/// on first use. Falls back to a hostname-derived identifier when the config
/// directory is unwritable.
fn persisted_receiver_id() -> Result<String> {
    let path = receiver_id_path();
    if let Some(saved) = std::fs::read_to_string(&path).ok().and_then(validate_uuid) {
        return Ok(saved);
    }
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("could not generate a receiver identifier")?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let id: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_ok()
        && std::fs::write(&path, &id).is_ok()
    {
        return Ok(id);
    }
    Ok(validate_uuid(id).unwrap_or_else(fallback_receiver_id))
}

fn receiver_id_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".config/cast/receiver-id"))
        .unwrap_or_else(|| PathBuf::from("/tmp/cast/receiver-id"))
}

fn validate_uuid(value: String) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    (value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(value)
}

fn fallback_receiver_id() -> String {
    let seed = hostname().unwrap_or_else(|| "cast-receiver".to_owned());
    let mut state = 0xcbf2_9ce4_8422_2325_u64;
    for byte in seed.bytes() {
        state ^= u64::from(byte);
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut id = String::with_capacity(32);
    for index in 0..4 {
        let chunk =
            state.rotate_left(index * 17) ^ (0x9e37_79b9_u64.wrapping_mul(index as u64 + 1));
        id.push_str(&format!("{chunk:016x}"));
    }
    id.truncate(32);
    id
}

/// Heuristic for `--capabilities auto`: prefer video whenever a desktop
/// session looks present, and stay audio-only on headless machines. macOS
/// machines in scope always have an attached display, so they stay video.
fn auto_capability() -> DeviceCapability {
    #[cfg(target_os = "linux")]
    {
        let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
        let has_display =
            std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
        if session_type.eq_ignore_ascii_case("x11")
            || session_type.eq_ignore_ascii_case("wayland")
            || has_display
        {
            DeviceCapability::Video
        } else {
            DeviceCapability::AudioOnly
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        DeviceCapability::Video
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_name_appends_cast_to_the_hostname() {
        let name = default_name();
        assert!(name.ends_with(" Cast"));
        assert!(name.len() > " Cast".len());
    }

    #[test]
    fn persisted_receiver_id_is_stable_and_well_formed() {
        let first = persisted_receiver_id().unwrap();
        let second = persisted_receiver_id().unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn uuid_validation_accepts_only_32_hex_characters() {
        assert!(validate_uuid("0123456789abcdef0123456789abcdef".to_owned()).is_some());
        assert!(validate_uuid("0123456789ABCDEF0123456789ABCDEF".to_owned()).is_some());
        assert!(validate_uuid("short".to_owned()).is_none());
        assert!(validate_uuid("0123456789abcdef0123456789abcdeg".to_owned()).is_none());
        assert!(validate_uuid("0123456789abcdef0123456789abcdef ".to_owned()).is_some());
    }

    #[test]
    fn fallback_receiver_id_is_deterministic_and_well_formed() {
        let first = fallback_receiver_id();
        let second = fallback_receiver_id();
        assert_eq!(first, second);
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn headless_environments_auto_detect_audio_only() {
        std::env::remove_var("DISPLAY");
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::remove_var("XDG_SESSION_TYPE");
        assert_eq!(auto_capability(), DeviceCapability::AudioOnly);
        std::env::set_var("DISPLAY", ":0");
        assert_eq!(auto_capability(), DeviceCapability::Video);
        std::env::remove_var("DISPLAY");
    }
}
