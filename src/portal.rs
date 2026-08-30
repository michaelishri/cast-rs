use std::{
    env, fs,
    io::Cursor,
    mem::{align_of, size_of},
    os::{fd::OwnedFd, unix::fs::PermissionsExt},
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
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

use crate::{
    linux_capture::{
        CaptureBackend, CaptureEpoch, CapturedFrame, CursorImage, CursorPixelFormat, FrameSink,
    },
    linux_encoder::RawPixelFormat,
    linux_pipewire::DequeuedBuffer,
};

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
        if self.cursor_metadata {
            Ok(CursorMode::Metadata)
        } else if self.cursor_embedded {
            Ok(CursorMode::Embedded)
        } else {
            bail!("this desktop portal offers no supported cursor capture mode")
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
    session: Option<Arc<Session<Screencast>>>,
    stream: PortalStream,
    cursor_mode: CursorMode,
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

    fn cursor_mode(&self) -> CursorMode {
        self.cursor_mode
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
        let Some(session) = self.session.take() else {
            return;
        };
        let result = self.runtime.block_on(session.close());
        drop_inside_runtime(&self.runtime, session);
        if let Err(error) = result {
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
    let runtime = runtime()?;
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
    let cursor_mode = capabilities.cursor_mode()?;
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
                .set_cursor_mode(cursor_mode)
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
        session: Some(session),
        stream,
        cursor_mode,
        remote: Some(remote),
        closed,
    })
}

fn runtime() -> Result<Arc<tokio::runtime::Runtime>> {
    static PORTAL_RUNTIME: OnceLock<Arc<tokio::runtime::Runtime>> = OnceLock::new();

    if let Some(runtime) = PORTAL_RUNTIME.get() {
        return Ok(Arc::clone(runtime));
    }
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("could not start the desktop portal runtime")?,
    );
    if PORTAL_RUNTIME.set(Arc::clone(&runtime)).is_err() {
        return Ok(Arc::clone(
            PORTAL_RUNTIME
                .get()
                .expect("another thread initialized the desktop portal runtime"),
        ));
    }
    Ok(runtime)
}

fn drop_inside_runtime<T>(runtime: &tokio::runtime::Runtime, value: T) {
    runtime.block_on(async move { drop(value) });
}

const fn availability(value: bool) -> &'static str {
    if value { "available" } else { "unavailable" }
}

struct PipeWireUserData {
    format: spa::param::video::VideoInfoRaw,
    sink: Arc<dyn FrameSink>,
    capture_epoch: CaptureEpoch,
    cursor_metadata: bool,
    last_cursor: Option<CursorImage>,
    failure: Arc<Mutex<Option<String>>>,
}

struct PipeWireVideoConfig {
    node_id: u32,
    sink: Arc<dyn FrameSink>,
    cursor_metadata: bool,
    capture_epoch: CaptureEpoch,
    failure: Arc<Mutex<Option<String>>>,
}

pub(crate) struct PipeWireCapture {
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
    _selection: PortalSelection,
    source_description: String,
}

impl PipeWireCapture {
    pub(crate) fn start_at(
        mut selection: PortalSelection,
        sink: Arc<dyn FrameSink>,
        capture_epoch: CaptureEpoch,
    ) -> Result<Self> {
        let source_description = match (selection.source_type(), selection.size()) {
            (Some(source_type), Some((width, height))) => {
                format!("portal {source_type:?} ({width}x{height})")
            }
            (Some(source_type), None) => format!("portal {source_type:?}"),
            (None, Some((width, height))) => format!("portal source ({width}x{height})"),
            (None, None) => "portal source".to_owned(),
        };
        let remote = selection.take_remote()?;
        let node_id = selection.node_id();
        let cursor_metadata = selection.cursor_mode() == CursorMode::Metadata;
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
                    PipeWireVideoConfig {
                        node_id,
                        sink,
                        cursor_metadata,
                        capture_epoch,
                        failure: Arc::clone(&thread_failure),
                    },
                    &thread_stop,
                    &portal_closed,
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
            source_description,
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

impl CaptureBackend for PipeWireCapture {
    fn source_description(&self) -> &str {
        &self.source_description
    }

    fn check(&self) -> Result<()> {
        Self::check(self)
    }

    fn stop(&mut self) -> Result<()> {
        Self::stop(self)
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
    config: PipeWireVideoConfig,
    stop: &Arc<AtomicBool>,
    portal_closed: &Arc<AtomicBool>,
) -> Result<()> {
    let PipeWireVideoConfig {
        node_id,
        sink,
        cursor_metadata,
        capture_epoch,
        failure,
    } = config;
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
            capture_epoch,
            cursor_metadata,
            last_cursor: None,
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
            let Some(mut buffer) = DequeuedBuffer::dequeue(stream) else {
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
    buffer: &mut DequeuedBuffer<'_>,
    user_data: &mut PipeWireUserData,
) -> Result<()> {
    let size = user_data.format.size();
    let width = size.width;
    let height = size.height;
    if width == 0 || height == 0 {
        return Ok(());
    }
    let format = raw_pixel_format(user_data.format.format())?;
    if user_data.cursor_metadata {
        update_cursor_metadata(
            buffer.metadata(spa::sys::SPA_META_Cursor),
            &mut user_data.last_cursor,
        )?;
    }
    let timestamp = user_data.capture_epoch.ticks(90_000);
    let data = buffer
        .datas_mut()
        .first_mut()
        .ok_or_else(|| anyhow!("PipeWire frame had no data plane"))?;
    let chunk = data.chunk();
    let offset = usize::try_from(chunk.offset())?;
    let chunk_bytes = usize::try_from(chunk.size())?;
    let signed_stride = chunk.stride();
    let mapped = data
        .data()
        .ok_or_else(|| anyhow!("PipeWire frame data was not memory-mapped"))?;
    // SPA defines chunk offsets modulo the data block's maximum size.
    let offset = if mapped.is_empty() {
        offset
    } else {
        offset % mapped.len()
    };
    let row_bytes = usize::try_from(width)?
        .checked_mul(4)
        .ok_or_else(|| anyhow!("PipeWire frame row size overflowed"))?;
    let available = mapped_frame_storage(mapped.len(), offset, signed_stride, row_bytes)?;
    log::trace!(
        "PipeWire video buffer: mapped={} bytes, chunk={} bytes, offset={}, stride={}",
        mapped.len(),
        chunk_bytes,
        offset,
        signed_stride
    );
    let (pixels, stride) = normalize_strided_image(
        mapped,
        offset,
        available,
        signed_stride,
        width,
        height,
        "PipeWire frame",
    )?;
    user_data.sink.submit(CapturedFrame {
        data: pixels,
        stride,
        width,
        height,
        format,
        timestamp,
        cursor: user_data
            .cursor_metadata
            .then_some(user_data.last_cursor.clone())
            .flatten(),
    });
    Ok(())
}

fn mapped_frame_storage(
    mapped_len: usize,
    offset: usize,
    signed_stride: i32,
    row_bytes: usize,
) -> Result<usize> {
    if offset >= mapped_len {
        bail!("PipeWire frame offset falls outside its mapped buffer");
    }
    if signed_stride < 0 {
        Ok(offset
            .checked_add(row_bytes)
            .ok_or_else(|| anyhow!("PipeWire frame storage range overflowed"))?
            .min(mapped_len))
    } else {
        Ok(mapped_len - offset)
    }
}

fn update_cursor_metadata(
    metadata: Option<&[u8]>,
    current: &mut Option<CursorImage>,
) -> Result<()> {
    let Some(metadata) = metadata else {
        return Ok(());
    };
    let cursor = metadata_struct::<spa::sys::spa_meta_cursor>(metadata, 0, "cursor")?;
    if cursor.id == 0 {
        return Ok(());
    }
    let position = (cursor.position.x, cursor.position.y);
    let hotspot = (cursor.hotspot.x, cursor.hotspot.y);
    if cursor.bitmap_offset == 0 {
        if let Some(image) = current {
            image.position = position;
            image.hotspot = hotspot;
        }
        return Ok(());
    }
    let bitmap_offset = usize::try_from(cursor.bitmap_offset)?;
    if bitmap_offset < size_of::<spa::sys::spa_meta_cursor>() {
        bail!("PipeWire cursor bitmap overlaps its cursor metadata");
    }
    let bitmap =
        metadata_struct::<spa::sys::spa_meta_bitmap>(metadata, bitmap_offset, "cursor bitmap")?;
    if bitmap.format == 0 {
        if let Some(image) = current {
            image.position = position;
            image.hotspot = hotspot;
        }
        return Ok(());
    }
    if bitmap.offset == 0 {
        *current = None;
        return Ok(());
    }
    let width = bitmap.size.width;
    let height = bitmap.size.height;
    let signed_stride = bitmap.stride;
    let stride = if signed_stride == 0 {
        usize::try_from(width)?.saturating_mul(4)
    } else {
        usize::try_from(signed_stride.unsigned_abs())?
    };
    let minimum_stride = usize::try_from(width)?.saturating_mul(4);
    if width == 0 || height == 0 || stride < minimum_stride {
        bail!("PipeWire cursor bitmap has invalid dimensions or stride");
    }
    let pixel_offset = bitmap_offset
        .checked_add(usize::try_from(bitmap.offset)?)
        .ok_or_else(|| anyhow!("PipeWire cursor bitmap offset overflowed"))?;
    let pixel_length = stride
        .checked_mul(usize::try_from(height)?)
        .ok_or_else(|| anyhow!("PipeWire cursor bitmap size overflowed"))?;
    let (pixels, stride) = normalize_strided_image(
        metadata,
        pixel_offset,
        pixel_length,
        signed_stride,
        width,
        height,
        "PipeWire cursor bitmap",
    )?;
    *current = Some(CursorImage {
        position,
        hotspot,
        width,
        height,
        stride,
        format: cursor_pixel_format(spa::param::video::VideoFormat::from_raw(bitmap.format))?,
        pixels,
    });
    Ok(())
}

fn metadata_struct<'a, T>(metadata: &'a [u8], offset: usize, label: &str) -> Result<&'a T> {
    let end = offset
        .checked_add(size_of::<T>())
        .ok_or_else(|| anyhow!("PipeWire {label} metadata range overflowed"))?;
    let bytes = metadata
        .get(offset..end)
        .ok_or_else(|| anyhow!("PipeWire {label} metadata is truncated"))?;
    if !(bytes.as_ptr() as usize).is_multiple_of(align_of::<T>()) {
        bail!("PipeWire {label} metadata is misaligned");
    }
    // SAFETY: The range and alignment were validated above. SPA metadata is a
    // C allocation containing the requested plain-old-data structure.
    Ok(unsafe { &*bytes.as_ptr().cast::<T>() })
}

fn normalize_strided_image(
    source: &[u8],
    offset: usize,
    available: usize,
    signed_stride: i32,
    width: u32,
    height: u32,
    label: &str,
) -> Result<(Vec<u8>, usize)> {
    if source.is_empty() {
        bail!("{label} has an empty mapped buffer");
    }
    let row_bytes = usize::try_from(width)?
        .checked_mul(4)
        .ok_or_else(|| anyhow!("{label} row size overflowed"))?;
    let height = usize::try_from(height)?;
    let stride = if signed_stride == 0 {
        row_bytes
    } else {
        usize::try_from(signed_stride.unsigned_abs())?
    };
    if row_bytes == 0 || height == 0 || stride < row_bytes {
        bail!("{label} has invalid dimensions or stride");
    }
    let span = stride
        .checked_mul(height.saturating_sub(1))
        .and_then(|prefix| prefix.checked_add(row_bytes))
        .ok_or_else(|| anyhow!("{label} row span overflowed"))?;
    if span > available {
        bail!("{label} is shorter than its dimensions and stride require");
    }

    let first_row = offset;
    let output_len = stride
        .checked_mul(height)
        .ok_or_else(|| anyhow!("{label} output size overflowed"))?;
    let mut output = vec![0; output_len];
    for row in 0..height {
        let distance = row
            .checked_mul(stride)
            .ok_or_else(|| anyhow!("{label} row offset overflowed"))?;
        let source_start = if signed_stride < 0 {
            first_row.checked_sub(distance)
        } else {
            first_row.checked_add(distance)
        }
        .ok_or_else(|| anyhow!("{label} row falls outside its mapped buffer"))?;
        let source_end = source_start
            .checked_add(row_bytes)
            .ok_or_else(|| anyhow!("{label} row range overflowed"))?;
        let source_row = source
            .get(source_start..source_end)
            .ok_or_else(|| anyhow!("{label} row exceeds its mapped buffer"))?;
        let destination_start = row * stride;
        output[destination_start..destination_start + row_bytes].copy_from_slice(source_row);
    }
    Ok((output, stride))
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

fn cursor_pixel_format(format: spa::param::video::VideoFormat) -> Result<CursorPixelFormat> {
    match format {
        value if value == spa::param::video::VideoFormat::RGBx => Ok(CursorPixelFormat::Rgbx),
        value if value == spa::param::video::VideoFormat::BGRx => Ok(CursorPixelFormat::Bgrx),
        value if value == spa::param::video::VideoFormat::xRGB => Ok(CursorPixelFormat::Xrgb),
        value if value == spa::param::video::VideoFormat::xBGR => Ok(CursorPixelFormat::Xbgr),
        value if value == spa::param::video::VideoFormat::RGBA => Ok(CursorPixelFormat::Rgba),
        value if value == spa::param::video::VideoFormat::BGRA => Ok(CursorPixelFormat::Bgra),
        value if value == spa::param::video::VideoFormat::ARGB => Ok(CursorPixelFormat::Argb),
        value if value == spa::param::video::VideoFormat::ABGR => Ok(CursorPixelFormat::Abgr),
        other => bail!("unsupported PipeWire cursor format {other:?}"),
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

    struct RuntimeDropProbe(Arc<AtomicBool>);

    impl Drop for RuntimeDropProbe {
        fn drop(&mut self) {
            self.0.store(
                tokio::runtime::Handle::try_current().is_ok(),
                Ordering::SeqCst,
            );
        }
    }

    #[test]
    fn portal_operations_share_one_process_runtime() {
        let first = runtime().unwrap();
        let second = runtime().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn runtime_bound_values_are_dropped_inside_the_portal_runtime() {
        let dropped_inside_runtime = Arc::new(AtomicBool::new(false));
        drop_inside_runtime(
            &runtime().unwrap(),
            RuntimeDropProbe(Arc::clone(&dropped_inside_runtime)),
        );
        assert!(dropped_inside_runtime.load(Ordering::SeqCst));
    }

    #[test]
    fn normalizes_positive_and_negative_video_strides() {
        let rows = [
            1, 1, 1, 1, 0, 0, 0, 0, 2, 2, 2, 2, 0, 0, 0, 0, 3, 3, 3, 3, 0, 0, 0, 0,
        ];
        let (positive, stride) =
            normalize_strided_image(&rows, 0, rows.len(), 8, 1, 3, "test image").unwrap();
        assert_eq!(stride, 8);
        assert_eq!(&positive[0..4], &[1; 4]);
        assert_eq!(&positive[8..12], &[2; 4]);
        assert_eq!(&positive[16..20], &[3; 4]);

        let (negative, stride) =
            normalize_strided_image(&rows, 16, rows.len(), -8, 1, 3, "test image").unwrap();
        assert_eq!(stride, 8);
        assert_eq!(&negative[0..4], &[3; 4]);
        assert_eq!(&negative[8..12], &[2; 4]);
        assert_eq!(&negative[16..20], &[1; 4]);
    }

    #[test]
    fn frame_storage_uses_mapped_plane_bounds_instead_of_chunk_payload_size() {
        let rows = [0_u8; 24];
        let available = mapped_frame_storage(rows.len(), 0, 8, 4).unwrap();
        assert_eq!(available, rows.len());
        normalize_strided_image(&rows, 0, available, 8, 1, 3, "test frame").unwrap();

        let available = mapped_frame_storage(rows.len(), 16, -8, 4).unwrap();
        assert_eq!(available, 20);
        normalize_strided_image(&rows, 16, available, -8, 1, 3, "test frame").unwrap();
        assert!(mapped_frame_storage(rows.len(), rows.len(), 8, 4).is_err());
    }

    #[test]
    fn rejects_truncated_and_out_of_range_strided_images() {
        let rows = [0_u8; 24];
        assert!(
            normalize_strided_image(&rows, 0, 12, 8, 1, 3, "test image")
                .unwrap_err()
                .to_string()
                .contains("shorter")
        );
        assert!(
            normalize_strided_image(&rows, 0, rows.len(), -8, 1, 3, "test image")
                .unwrap_err()
                .to_string()
                .contains("outside")
        );
    }

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
        assert_eq!(capabilities.cursor_mode().unwrap(), CursorMode::Metadata);

        let embedded_only = PortalCapabilities {
            cursor_metadata: false,
            ..capabilities
        };
        assert_eq!(embedded_only.cursor_mode().unwrap(), CursorMode::Embedded);

        let no_cursor = PortalCapabilities {
            cursor_metadata: false,
            cursor_embedded: false,
            ..capabilities
        };
        assert!(no_cursor.cursor_mode().is_err());
    }

    #[test]
    fn cursor_metadata_updates_bitmap_and_position_safely() {
        #[repr(C)]
        struct CursorFixture {
            cursor: spa::sys::spa_meta_cursor,
            bitmap: spa::sys::spa_meta_bitmap,
            pixels: [u8; 16],
        }

        let bitmap_offset = std::mem::offset_of!(CursorFixture, bitmap);
        let pixel_offset = std::mem::offset_of!(CursorFixture, pixels) - bitmap_offset;
        let fixture = CursorFixture {
            cursor: spa::sys::spa_meta_cursor {
                id: 1,
                flags: 0,
                position: spa::sys::spa_point { x: 4, y: 5 },
                hotspot: spa::sys::spa_point { x: 1, y: 1 },
                bitmap_offset: bitmap_offset as u32,
            },
            bitmap: spa::sys::spa_meta_bitmap {
                format: spa::param::video::VideoFormat::BGRA.as_raw(),
                size: spa::sys::spa_rectangle {
                    width: 2,
                    height: 2,
                },
                stride: 8,
                offset: pixel_offset as u32,
            },
            pixels: [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255],
        };
        // SAFETY: The fixture is repr(C), alive for the slice lifetime, and the
        // byte length covers the complete allocation.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(&fixture).cast::<u8>(),
                size_of::<CursorFixture>(),
            )
        };
        let mut current = None;
        update_cursor_metadata(Some(bytes), &mut current).unwrap();
        let image = current.as_ref().unwrap();
        assert_eq!(image.position, (4, 5));
        assert_eq!(image.hotspot, (1, 1));
        assert_eq!((image.width, image.height, image.stride), (2, 2, 8));
        assert_eq!(image.format, CursorPixelFormat::Bgra);
        assert_eq!(image.pixels, fixture.pixels);

        let moved = spa::sys::spa_meta_cursor {
            id: 1,
            flags: 0,
            position: spa::sys::spa_point { x: 8, y: 9 },
            hotspot: spa::sys::spa_point { x: 2, y: 3 },
            bitmap_offset: 0,
        };
        // SAFETY: `moved` is alive, correctly aligned, and represented by the
        // exact size of the C metadata structure.
        let moved_bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(&moved).cast::<u8>(),
                size_of::<spa::sys::spa_meta_cursor>(),
            )
        };
        update_cursor_metadata(Some(moved_bytes), &mut current).unwrap();
        let image = current.as_ref().unwrap();
        assert_eq!(image.position, (8, 9));
        assert_eq!(image.hotspot, (2, 3));
        assert_eq!(image.pixels, fixture.pixels);

        let unchanged = spa::sys::spa_meta_cursor { id: 0, ..moved };
        // SAFETY: `unchanged` is alive and represented by the exact size of the
        // C metadata structure.
        let unchanged_bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(&unchanged).cast::<u8>(),
                size_of::<spa::sys::spa_meta_cursor>(),
            )
        };
        update_cursor_metadata(Some(unchanged_bytes), &mut current).unwrap();
        assert_eq!(current.as_ref().unwrap().position, (8, 9));

        let mut hidden = fixture;
        hidden.cursor.position = spa::sys::spa_point { x: 10, y: 11 };
        hidden.bitmap.offset = 0;
        // SAFETY: `hidden` is repr(C), alive for the slice lifetime, and the
        // byte length covers the complete allocation.
        let hidden_bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(&hidden).cast::<u8>(),
                size_of::<CursorFixture>(),
            )
        };
        update_cursor_metadata(Some(hidden_bytes), &mut current).unwrap();
        assert!(current.is_none());
        assert!(update_cursor_metadata(Some(&[0; 4]), &mut current).is_err());
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

    #[test]
    fn packed_cursor_formats_are_mapped_explicitly() {
        let cases = [
            (
                spa::param::video::VideoFormat::RGBx,
                CursorPixelFormat::Rgbx,
                (1, 2, 3, 255),
            ),
            (
                spa::param::video::VideoFormat::BGRx,
                CursorPixelFormat::Bgrx,
                (3, 2, 1, 255),
            ),
            (
                spa::param::video::VideoFormat::xRGB,
                CursorPixelFormat::Xrgb,
                (2, 3, 4, 255),
            ),
            (
                spa::param::video::VideoFormat::xBGR,
                CursorPixelFormat::Xbgr,
                (4, 3, 2, 255),
            ),
            (
                spa::param::video::VideoFormat::RGBA,
                CursorPixelFormat::Rgba,
                (1, 2, 3, 4),
            ),
            (
                spa::param::video::VideoFormat::BGRA,
                CursorPixelFormat::Bgra,
                (3, 2, 1, 4),
            ),
            (
                spa::param::video::VideoFormat::ARGB,
                CursorPixelFormat::Argb,
                (2, 3, 4, 1),
            ),
            (
                spa::param::video::VideoFormat::ABGR,
                CursorPixelFormat::Abgr,
                (4, 3, 2, 1),
            ),
        ];
        for (spa, expected, rgba) in cases {
            let mapped = cursor_pixel_format(spa).unwrap();
            assert_eq!(mapped, expected);
            assert_eq!(mapped.rgba(&[1, 2, 3, 4]), rgba);
        }
        assert!(cursor_pixel_format(spa::param::video::VideoFormat::NV12).is_err());
    }
}
