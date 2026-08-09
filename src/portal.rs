use std::{
    env, fs,
    io::Cursor,
    os::{fd::OwnedFd, unix::fs::PermissionsExt},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::{
    PersistMode, Session,
    screencast::{
        CursorMode, Screencast, SelectSourcesOptions, SourceType, Stream as PortalStream,
    },
};
use pipewire as pw;
use pw::{properties::properties, spa};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::linux_encoder::RawPixelFormat;

const TOKEN_VERSION: u8 = 1;
const TOKEN_FILE: &str = "screencast-source.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PortalSourceKind {
    Normal,
    #[allow(dead_code)] // Wired to the shared mirroring transport with --extend.
    Virtual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PortalCapabilities {
    pub(crate) monitor: bool,
    pub(crate) window: bool,
    pub(crate) virtual_source: bool,
    pub(crate) cursor_metadata: bool,
    pub(crate) cursor_embedded: bool,
}

impl PortalCapabilities {
    fn source_types(self, kind: PortalSourceKind) -> Result<enumflags2::BitFlags<SourceType>> {
        match kind {
            PortalSourceKind::Virtual if self.virtual_source => Ok(SourceType::Virtual.into()),
            PortalSourceKind::Virtual => bail!(
                "this desktop portal does not support virtual displays; use normal desktop sharing without --extend"
            ),
            PortalSourceKind::Normal => {
                let mut sources = enumflags2::BitFlags::empty();
                if self.monitor {
                    sources |= SourceType::Monitor;
                }
                if self.window {
                    sources |= SourceType::Window;
                }
                if sources.is_empty() {
                    bail!("this desktop portal offers neither monitor nor window capture");
                }
                Ok(sources)
            }
        }
    }

    fn cursor_mode(self) -> Result<CursorMode> {
        if self.cursor_embedded {
            Ok(CursorMode::Embedded)
        } else {
            bail!(
                "this desktop portal does not offer embedded cursor capture (metadata-only cursors require a newer PipeWire runtime)"
            )
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct StoredToken {
    version: u8,
    token: String,
}

#[derive(Clone, Debug)]
struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    fn discover() -> Result<Self> {
        let state = if let Some(path) = env::var_os("XDG_STATE_HOME") {
            PathBuf::from(path)
        } else {
            let home = env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| anyhow!("HOME is not set; set XDG_STATE_HOME for portal state"))?;
            home.join(".local/state")
        };
        Ok(Self {
            path: state.join("cast").join(TOKEN_FILE),
        })
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self { path }
    }

    fn load(&self) -> Result<Option<String>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not read {}", self.path.display()));
            }
        };
        let stored: StoredToken = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "remembered portal source {} is corrupt",
                self.path.display()
            )
        })?;
        if stored.version != TOKEN_VERSION || stored.token.trim().is_empty() {
            bail!(
                "remembered portal source {} is invalid",
                self.path.display()
            );
        }
        Ok(Some(stored.token))
    }

    fn load_or_forget_corrupt(&self) -> Option<String> {
        match self.load() {
            Ok(token) => token,
            Err(error) => {
                log::warn!("{error:#}; opening the source chooser instead");
                if let Err(remove_error) = self.remove() {
                    log::warn!("could not remove corrupt portal state: {remove_error:#}");
                }
                None
            }
        }
    }

    fn save(&self, token: &str) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("portal token path has no parent"))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("could not secure {}", parent.display()))?;
        let mut temporary = NamedTempFile::new_in(parent).with_context(|| {
            format!(
                "could not create temporary portal state in {}",
                parent.display()
            )
        })?;
        temporary
            .as_file_mut()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("could not secure temporary portal state")?;
        serde_json::to_writer(
            temporary.as_file_mut(),
            &StoredToken {
                version: TOKEN_VERSION,
                token: token.to_owned(),
            },
        )
        .context("could not serialize the portal restore token")?;
        temporary
            .as_file_mut()
            .sync_all()
            .context("could not sync the portal restore token")?;
        temporary
            .persist(&self.path)
            .map_err(|error| error.error)
            .with_context(|| format!("could not atomically save {}", self.path.display()))?;
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("could not secure {}", self.path.display()))?;
        Ok(())
    }

    fn remove(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("could not remove {}", self.path.display()))
            }
        }
    }

    fn exists(&self) -> bool {
        self.load().ok().flatten().is_some()
    }
}

pub(crate) struct PortalSelection {
    runtime: Arc<tokio::runtime::Runtime>,
    session: Arc<Session<Screencast>>,
    stream: PortalStream,
    remote: Option<OwnedFd>,
    closed: Arc<AtomicBool>,
}

impl PortalSelection {
    pub(crate) fn node_id(&self) -> u32 {
        self.stream.pipe_wire_node_id()
    }

    pub(crate) fn source_type(&self) -> Option<SourceType> {
        self.stream.source_type()
    }

    pub(crate) fn size(&self) -> Option<(i32, i32)> {
        self.stream.size()
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("the desktop portal session closed unexpectedly");
        }
        Ok(())
    }

    fn take_remote(&mut self) -> Result<OwnedFd> {
        self.remote
            .take()
            .ok_or_else(|| anyhow!("PipeWire remote was already consumed"))
    }
}

impl Drop for PortalSelection {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Err(error) = self.runtime.block_on(self.session.close()) {
            log::warn!("could not close the desktop portal session: {error}");
        }
    }
}

pub(crate) fn capabilities() -> Result<PortalCapabilities> {
    let runtime = runtime()?;
    runtime.block_on(async {
        let portal = Screencast::new()
            .await
            .context("could not connect to the XDG ScreenCast portal")?;
        capabilities_with(&portal).await
    })
}

pub(crate) fn list_sources(select_source: bool) -> Result<()> {
    let capabilities = capabilities()?;
    let remembered = TokenStore::discover()?.exists();
    println!("XDG ScreenCast portal capabilities:");
    println!("  Monitor sources: {}", availability(capabilities.monitor));
    println!("  Window sources: {}", availability(capabilities.window));
    println!(
        "  Virtual sources: {}",
        availability(capabilities.virtual_source)
    );
    println!(
        "  Cursor metadata: {}",
        availability(capabilities.cursor_metadata)
    );
    println!(
        "  Remembered source: {}",
        if remembered { "yes" } else { "no" }
    );
    if select_source {
        let selection = select(PortalSourceKind::Normal, true)?;
        println!(
            "Saved {:?} source (PipeWire node {}, compositor size {:?}).",
            selection.source_type(),
            selection.node_id(),
            selection.size()
        );
    }
    Ok(())
}

pub(crate) fn select(kind: PortalSourceKind, force_chooser: bool) -> Result<PortalSelection> {
    let runtime = Arc::new(runtime()?);
    let store = TokenStore::discover()?;
    let restore_token = (!force_chooser && kind == PortalSourceKind::Normal)
        .then(|| store.load_or_forget_corrupt())
        .flatten();
    open_with_restore_retry(restore_token.as_deref(), &store, |token| {
        runtime.block_on(open_selection(&runtime, kind, token, &store))
    })
}

fn open_with_restore_retry<T>(
    restore_token: Option<&str>,
    store: &TokenStore,
    mut open: impl FnMut(Option<&str>) -> Result<T>,
) -> Result<T> {
    match open(restore_token) {
        Ok(selection) => Ok(selection),
        Err(error) if restore_token.is_some() => {
            log::warn!(
                "remembered portal source could not be restored: {error:#}; opening the chooser"
            );
            store.remove()?;
            open(None)
        }
        Err(error) => Err(error),
    }
}

async fn capabilities_with(portal: &Screencast) -> Result<PortalCapabilities> {
    let sources = portal
        .available_source_types()
        .await
        .context("could not query portal source capabilities")?;
    let cursors = portal
        .available_cursor_modes()
        .await
        .context("could not query portal cursor capabilities")?;
    Ok(PortalCapabilities {
        monitor: sources.contains(SourceType::Monitor),
        window: sources.contains(SourceType::Window),
        virtual_source: sources.contains(SourceType::Virtual),
        cursor_metadata: cursors.contains(CursorMode::Metadata),
        cursor_embedded: cursors.contains(CursorMode::Embedded),
    })
}

async fn open_selection(
    runtime: &Arc<tokio::runtime::Runtime>,
    kind: PortalSourceKind,
    restore_token: Option<&str>,
    store: &TokenStore,
) -> Result<PortalSelection> {
    let portal = Screencast::new()
        .await
        .context("could not connect to the XDG ScreenCast portal")?;
    let capabilities = capabilities_with(&portal).await?;
    let session = Arc::new(
        portal
            .create_session(Default::default())
            .await
            .context("could not create a desktop portal session")?,
    );
    portal
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(capabilities.cursor_mode()?)
                .set_sources(capabilities.source_types(kind)?)
                .set_multiple(false)
                .set_restore_token(restore_token)
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
        )
        .await
        .context("desktop source selection was denied")?
        .response()
        .context("desktop source selection was cancelled or denied")?;
    let response = portal
        .start(&session, None, Default::default())
        .await
        .context("could not start desktop portal selection")?
        .response()
        .context("desktop source chooser was cancelled or denied")?;
    let stream = response
        .streams()
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("the desktop portal returned no selected stream"))?;
    if let Some(token) = response.restore_token()
        && kind == PortalSourceKind::Normal
    {
        store.save(token)?;
    }
    let remote = portal
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("could not open the portal PipeWire remote")?;
    let closed = Arc::new(AtomicBool::new(false));
    let watcher_session = Arc::clone(&session);
    let watcher_closed = Arc::clone(&closed);
    runtime.spawn(async move {
        match watcher_session.receive_closed().await {
            Ok(mut events) => {
                use futures_lite::StreamExt;
                let _ = events.next().await;
                watcher_closed.store(true, Ordering::SeqCst);
            }
            Err(error) => log::warn!("could not watch the portal session: {error}"),
        }
    });
    Ok(PortalSelection {
        runtime: Arc::clone(runtime),
        session,
        stream,
        remote: Some(remote),
        closed,
    })
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not start the desktop portal runtime")
}

const fn availability(value: bool) -> &'static str {
    if value { "available" } else { "unavailable" }
}

#[derive(Clone, Debug)]
pub(crate) struct CursorImage {
    pub(crate) position: (i32, i32),
    pub(crate) hotspot: (i32, i32),
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: usize,
    pub(crate) format: RawPixelFormat,
    pub(crate) pixels: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct CapturedFrame {
    pub(crate) data: Vec<u8>,
    pub(crate) stride: usize,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) format: RawPixelFormat,
    pub(crate) timestamp: u64,
    pub(crate) cursor: Option<CursorImage>,
}

pub(crate) trait FrameSink: Send + Sync + 'static {
    fn submit(&self, frame: CapturedFrame);
}

struct PipeWireUserData {
    format: spa::param::video::VideoInfoRaw,
    sink: Arc<dyn FrameSink>,
    capture_started: Instant,
    failure: Arc<Mutex<Option<String>>>,
}

pub(crate) struct PipeWireCapture {
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
    _selection: PortalSelection,
}

impl PipeWireCapture {
    pub(crate) fn start(mut selection: PortalSelection, sink: Arc<dyn FrameSink>) -> Result<Self> {
        let remote = selection.take_remote()?;
        let node_id = selection.node_id();
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let thread_stop = Arc::clone(&stop);
        let thread_failure = Arc::clone(&failure);
        let portal_closed = Arc::clone(&selection.closed);
        let thread = thread::Builder::new()
            .name("cast-pipewire-video".to_owned())
            .spawn(move || {
                if let Err(error) = run_pipewire(
                    remote,
                    node_id,
                    sink,
                    &thread_stop,
                    &portal_closed,
                    Arc::clone(&thread_failure),
                ) && let Ok(mut failure) = thread_failure.lock()
                    && failure.is_none()
                {
                    *failure = Some(format!("{error:#}"));
                }
            })
            .context("could not start the PipeWire capture thread")?;
        Ok(Self {
            stop,
            failure,
            thread: Some(thread),
            _selection: selection,
        })
    }

    pub(crate) fn check(&self) -> Result<()> {
        if let Some(error) = self
            .failure
            .lock()
            .map_err(|_| anyhow!("PipeWire failure lock was poisoned"))?
            .as_ref()
        {
            bail!("PipeWire desktop capture failed: {error}");
        }
        Ok(())
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("PipeWire capture thread panicked"))?;
        }
        self.check()
    }
}

impl Drop for PipeWireCapture {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            log::warn!("could not stop PipeWire desktop capture: {error:#}");
        }
    }
}

fn run_pipewire(
    remote: OwnedFd,
    node_id: u32,
    sink: Arc<dyn FrameSink>,
    stop: &Arc<AtomicBool>,
    portal_closed: &Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
) -> Result<()> {
    pw::init();
    let mainloop =
        pw::main_loop::MainLoopRc::new(None).context("could not create PipeWire loop")?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .context("could not create PipeWire context")?;
    let core = context
        .connect_fd_rc(remote, None)
        .context("could not connect to the portal PipeWire remote")?;
    let stream = pw::stream::StreamRc::new(
        core,
        "cast desktop video",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .context("could not create the PipeWire video stream")?;
    let loop_for_state = mainloop.downgrade();
    let listener = stream
        .add_local_listener_with_user_data(PipeWireUserData {
            format: Default::default(),
            sink,
            capture_started: Instant::now(),
            failure: Arc::clone(&failure),
        })
        .state_changed(move |_, user_data, _, state| {
            if let pw::stream::StreamState::Error(message) = state {
                store_failure(
                    &user_data.failure,
                    format!("PipeWire stream error: {message}"),
                );
                if let Some(mainloop) = loop_for_state.upgrade() {
                    mainloop.quit();
                }
            }
        })
        .param_changed(|_, user_data, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != spa::param::format::MediaType::Video
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                store_failure(
                    &user_data.failure,
                    "portal negotiated a non-raw video stream",
                );
                return;
            }
            if let Err(error) = user_data.format.parse(param) {
                store_failure(
                    &user_data.failure,
                    format!("could not parse PipeWire video format: {error:?}"),
                );
            }
        })
        .process(|stream, user_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            if let Err(error) = copy_pipewire_frame(&mut buffer, user_data) {
                store_failure(
                    &user_data.failure,
                    format!("could not copy PipeWire frame: {error:#}"),
                );
            }
        })
        .register()
        .context("could not register PipeWire callbacks")?;

    let values = video_format_parameter()?;
    let mut params = [spa::pod::Pod::from_bytes(&values)
        .ok_or_else(|| anyhow!("could not construct PipeWire video parameters"))?];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("could not connect the PipeWire video node")?;
    let loop_for_timer = mainloop.downgrade();
    let timer_stop = Arc::clone(stop);
    let timer_closed = Arc::clone(portal_closed);
    let timer = mainloop.loop_().add_timer(move |_| {
        if (timer_stop.load(Ordering::SeqCst) || timer_closed.load(Ordering::SeqCst))
            && let Some(mainloop) = loop_for_timer.upgrade()
        {
            mainloop.quit();
        }
    });
    timer
        .update_timer(
            Some(Duration::from_millis(100)),
            Some(Duration::from_millis(100)),
        )
        .into_result()
        .context("could not arm the PipeWire shutdown timer")?;
    mainloop.run();
    drop(timer);
    drop(listener);
    stream
        .disconnect()
        .context("could not disconnect PipeWire stream")?;
    if portal_closed.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
        bail!("the desktop portal session closed unexpectedly");
    }
    Ok(())
}

fn video_format_parameter() -> Result<Vec<u8>> {
    let object = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRA,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::RGBx
        )
    );
    pw::spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(object),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|error| anyhow!("could not serialize PipeWire video parameters: {error:?}"))
}

fn copy_pipewire_frame(
    buffer: &mut pw::buffer::Buffer<'_>,
    user_data: &mut PipeWireUserData,
) -> Result<()> {
    let size = user_data.format.size();
    let width = size.width;
    let height = size.height;
    if width == 0 || height == 0 {
        return Ok(());
    }
    let format = raw_pixel_format(user_data.format.format())?;
    let timestamp = u64::try_from(user_data.capture_started.elapsed().as_nanos())
        .unwrap_or(u64::MAX)
        .saturating_mul(90_000)
        / 1_000_000_000;
    let data = buffer
        .datas_mut()
        .first_mut()
        .ok_or_else(|| anyhow!("PipeWire frame had no data plane"))?;
    let chunk = data.chunk();
    let offset = usize::try_from(chunk.offset())?;
    let bytes = usize::try_from(chunk.size())?;
    let stride = if chunk.stride() == 0 {
        usize::try_from(width)?.saturating_mul(4)
    } else {
        usize::try_from(chunk.stride().unsigned_abs())?
    };
    let mapped = data
        .data()
        .ok_or_else(|| anyhow!("PipeWire frame data was not memory-mapped"))?;
    let end = offset
        .checked_add(bytes)
        .ok_or_else(|| anyhow!("PipeWire frame size overflowed"))?;
    if end > mapped.len() || stride < usize::try_from(width)?.saturating_mul(4) {
        bail!("PipeWire frame dimensions exceed its mapped buffer");
    }
    user_data.sink.submit(CapturedFrame {
        data: mapped[offset..end].to_vec(),
        stride,
        width,
        height,
        format,
        timestamp,
        cursor: None,
    });
    Ok(())
}

fn raw_pixel_format(format: spa::param::video::VideoFormat) -> Result<RawPixelFormat> {
    match format {
        value if value == spa::param::video::VideoFormat::BGRA => Ok(RawPixelFormat::Bgra),
        value if value == spa::param::video::VideoFormat::BGRx => Ok(RawPixelFormat::Bgrx),
        value if value == spa::param::video::VideoFormat::RGBA => Ok(RawPixelFormat::Rgba),
        value if value == spa::param::video::VideoFormat::RGBx => Ok(RawPixelFormat::Rgbx),
        other => bail!("unsupported PipeWire video format {other:?}"),
    }
}

fn store_failure(failure: &Mutex<Option<String>>, message: impl Into<String>) {
    if let Ok(mut failure) = failure.lock()
        && failure.is_none()
    {
        *failure = Some(message.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trip_is_owner_only_and_atomic() {
        let directory = tempfile::tempdir().unwrap();
        let store = TokenStore::at(directory.path().join("cast").join(TOKEN_FILE));
        store.save("opaque token").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("opaque token"));
        assert_eq!(
            fs::metadata(&store.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(store.path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn corrupt_and_old_tokens_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let store = TokenStore::at(directory.path().join(TOKEN_FILE));
        fs::write(&store.path, b"not json").unwrap();
        assert!(store.load().unwrap_err().to_string().contains("corrupt"));
        fs::write(&store.path, br#"{"version":9,"token":"old"}"#).unwrap();
        assert!(store.load().unwrap_err().to_string().contains("invalid"));
    }

    #[test]
    fn corrupt_remembered_source_is_forgotten_before_the_chooser() {
        let directory = tempfile::tempdir().unwrap();
        let store = TokenStore::at(directory.path().join(TOKEN_FILE));
        fs::write(&store.path, b"truncated").unwrap();
        assert_eq!(store.load_or_forget_corrupt(), None);
        assert!(!store.path.exists());
    }

    #[test]
    fn chooser_outcomes_and_expired_restore_retry_are_distinct() {
        let directory = tempfile::tempdir().unwrap();
        let store = TokenStore::at(directory.path().join(TOKEN_FILE));
        let mut attempts = Vec::new();
        let selected = open_with_restore_retry(Some("expired"), &store, |token| {
            attempts.push(token.map(str::to_owned));
            if token.is_some() {
                bail!("expired restore token");
            }
            Ok("selected")
        })
        .unwrap();
        assert_eq!(selected, "selected");
        assert_eq!(attempts, [Some("expired".into()), None]);

        let mut cancelled_attempts = 0;
        let cancelled = open_with_restore_retry::<()>(None, &store, |_| {
            cancelled_attempts += 1;
            bail!("chooser cancelled")
        });
        assert!(cancelled.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(cancelled_attempts, 1);

        let denied = open_with_restore_retry::<()>(None, &store, |_| bail!("chooser denied"));
        assert!(denied.unwrap_err().to_string().contains("denied"));
    }

    #[test]
    fn source_plan_is_capability_gated() {
        let capabilities = PortalCapabilities {
            monitor: true,
            window: false,
            virtual_source: false,
            cursor_metadata: true,
            cursor_embedded: true,
        };
        assert_eq!(
            capabilities.source_types(PortalSourceKind::Normal).unwrap(),
            SourceType::Monitor
        );
        assert!(
            capabilities
                .source_types(PortalSourceKind::Virtual)
                .is_err()
        );
        assert_eq!(capabilities.cursor_mode().unwrap(), CursorMode::Embedded);

        let metadata_only = PortalCapabilities {
            cursor_embedded: false,
            ..capabilities
        };
        assert!(
            metadata_only
                .cursor_mode()
                .unwrap_err()
                .to_string()
                .contains("embedded cursor capture")
        );
    }

    #[test]
    fn packed_pipewire_formats_are_mapped_explicitly() {
        assert_eq!(
            raw_pixel_format(spa::param::video::VideoFormat::BGRA).unwrap(),
            RawPixelFormat::Bgra
        );
        assert_eq!(
            raw_pixel_format(spa::param::video::VideoFormat::RGBx).unwrap(),
            RawPixelFormat::Rgbx
        );
        assert!(raw_pixel_format(spa::param::video::VideoFormat::NV12).is_err());
    }
}
