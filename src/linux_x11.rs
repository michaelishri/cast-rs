use std::{
    collections::HashSet,
    env,
    os::fd::AsRawFd,
    ptr::NonNull,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{
        randr::{
            Connection as RandrConnection, ConnectionExt as _, ModeFlag, ModeInfo, Rotation,
            SetConfig,
        },
        shm::{self, ConnectionExt as _},
        xfixes::ConnectionExt as _,
        xproto::{ConnectionExt as _, ImageFormat, ImageOrder},
    },
    rust_connection::RustConnection,
};

use crate::{
    linux_capture::{
        CaptureBackend, CaptureEpoch, CapturedFrame, CursorImage, CursorPixelFormat, FrameSink,
    },
    linux_encoder::RawPixelFormat,
};

const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;
static X11_LAYOUT: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum BackendPreference {
    #[default]
    Auto,
    X11,
    Portal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backend {
    X11,
    Portal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Monitor {
    pub(crate) name: String,
    pub(crate) x: i16,
    pub(crate) y: i16,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) primary: bool,
}

impl Monitor {
    pub(crate) fn geometry(&self) -> String {
        format!("{}x{}{:+}{:+}", self.width, self.height, self.x, self.y)
    }
}

pub(crate) struct DisplayConnection {
    pub(crate) connection: RustConnection,
    pub(crate) screen_number: usize,
}

impl DisplayConnection {
    pub(crate) fn connect() -> Result<Self> {
        let display = env::var("DISPLAY").map_err(|_| {
            anyhow!("DISPLAY is not set; run cast inside the logged-in X11 desktop session or pass --backend portal")
        })?;
        let (connection, screen_number) = RustConnection::connect(Some(&display)).with_context(|| {
            format!(
                "could not connect to X11 display {display}; check DISPLAY and Xauthority access"
            )
        })?;
        Ok(Self {
            connection,
            screen_number,
        })
    }

    pub(crate) fn root(&self) -> u32 {
        self.connection.setup().roots[self.screen_number].root
    }

    fn screen(&self) -> &x11rb::protocol::xproto::Screen {
        &self.connection.setup().roots[self.screen_number]
    }

    pub(crate) fn monitors(&self) -> Result<Vec<Monitor>> {
        self.connection
            .randr_query_version(1, 5)
            .context("could not query the X11 RandR extension")?
            .reply()
            .context("the X11 server does not provide RandR monitor discovery")?;
        let reply = self
            .connection
            .randr_get_monitors(self.root(), true)
            .context("could not request active X11 RandR monitors")?
            .reply()
            .context("could not read active X11 RandR monitors")?;
        let mut monitors = Vec::with_capacity(reply.monitors.len());
        for monitor in reply.monitors {
            let name = self
                .connection
                .get_atom_name(monitor.name)
                .context("could not request an X11 monitor name")?
                .reply()
                .context("could not read an X11 monitor name")?;
            let name = String::from_utf8(name.name)
                .context("an X11 RandR monitor name was not valid UTF-8")?;
            monitors.push(Monitor {
                name,
                x: monitor.x,
                y: monitor.y,
                width: monitor.width,
                height: monitor.height,
                primary: monitor.primary,
            });
        }
        if monitors.is_empty() {
            bail!("the X11 server reported no active RandR monitors");
        }
        monitors
            .sort_by_key(|monitor| (!monitor.primary, monitor.y, monitor.x, monitor.name.clone()));
        Ok(monitors)
    }

    pub(crate) fn select_monitor(&self, requested: Option<&str>) -> Result<Monitor> {
        let monitors = self.monitors()?;
        match requested {
            Some(name) => monitors
                .into_iter()
                .find(|monitor| monitor.name == name)
                .ok_or_else(|| anyhow!(
                    "X11 monitor {name:?} was not found; run `cast displays --backend x11` to list active monitors"
                )),
            None => monitors
                .iter()
                .find(|monitor| monitor.primary)
                .or_else(|| monitors.first())
                .cloned()
                .ok_or_else(|| anyhow!("the X11 server reported no active monitors")),
        }
    }
}

/// A temporary RandR output used as an off-screen extended desktop.
pub(crate) struct X11VirtualDisplay {
    display: DisplayConnection,
    output: u32,
    crtc: u32,
    mode: u32,
    monitor_name: String,
    stopped: bool,
}

impl X11VirtualDisplay {
    pub(crate) fn start(width: u32, height: u32, ordinal: u32) -> Result<Self> {
        let width = u16::try_from(width).context("extended display width exceeds X11 limits")?;
        let height = u16::try_from(height).context("extended display height exceeds X11 limits")?;
        let _layout = X11_LAYOUT
            .lock()
            .map_err(|_| anyhow!("X11 layout lock was poisoned"))?;
        let display = DisplayConnection::connect()?;
        let root = display.root();
        let resources = display
            .connection
            .randr_get_screen_resources_current(root)?
            .reply()
            .context("could not read current X11 output resources")?;
        let active_crtcs = resources
            .crtcs
            .iter()
            .copied()
            .filter_map(|crtc| {
                display
                    .connection
                    .randr_get_crtc_info(crtc, resources.config_timestamp)
                    .ok()?
                    .reply()
                    .ok()
                    .filter(|info| info.mode != 0)
                    .map(|_| crtc)
            })
            .collect::<HashSet<_>>();

        let mut selected = None;
        for output in &resources.outputs {
            let info = display
                .connection
                .randr_get_output_info(*output, resources.config_timestamp)?
                .reply()?;
            if info.connection != RandrConnection::DISCONNECTED || info.crtc != 0 {
                continue;
            }
            if let Some(crtc) = info
                .crtcs
                .iter()
                .copied()
                .find(|crtc| !active_crtcs.contains(crtc))
            {
                let name =
                    String::from_utf8(info.name).context("an X11 output name was not UTF-8")?;
                selected = Some((*output, crtc, name));
                break;
            }
        }
        let (output, crtc, monitor_name) = selected.ok_or_else(|| anyhow!(
            "X11 cannot create another extended desktop: no unused disconnected output with a free CRTC is available"
        ))?;

        let monitors = display.monitors()?;
        let x = monitors
            .iter()
            .map(|m| i32::from(m.x) + i32::from(m.width))
            .max()
            .unwrap_or(0);
        let y = monitors
            .iter()
            .find(|m| m.primary)
            .map_or(0, |m| i32::from(m.y));
        let new_width = u16::try_from(
            x.checked_add(i32::from(width))
                .ok_or_else(|| anyhow!("extended desktop width overflowed"))?,
        )
        .context("extended desktop exceeds the X11 coordinate range")?;
        let new_height = u16::try_from(
            (y + i32::from(height)).max(i32::from(display.screen().height_in_pixels)),
        )
        .context("extended desktop exceeds the X11 coordinate range")?;
        let range = display
            .connection
            .randr_get_screen_size_range(root)?
            .reply()?;
        if new_width > range.max_width || new_height > range.max_height {
            bail!(
                "extended desktop {new_width}x{new_height} exceeds the X11 maximum {}x{}",
                range.max_width,
                range.max_height
            );
        }

        let mode_name = format!("CAST-{width}x{height}-{ordinal}-{}", std::process::id());
        let mode_info = generated_mode(width, height, mode_name.len())?;
        let mode = display
            .connection
            .randr_create_mode(root, mode_info, mode_name.as_bytes())?
            .reply()
            .context("X11 rejected the temporary extended-display mode")?
            .mode;
        let setup = (|| -> Result<()> {
            display
                .connection
                .randr_add_output_mode(output, mode)?
                .check()
                .context("could not attach the temporary mode to the unused X11 output")?;
            let screen = display.screen();
            let mm_width = pixels_to_mm(
                new_width,
                screen.width_in_pixels,
                screen.width_in_millimeters,
            );
            let mm_height = pixels_to_mm(
                new_height,
                screen.height_in_pixels,
                screen.height_in_millimeters,
            );
            display
                .connection
                .randr_set_screen_size(root, new_width, new_height, mm_width, mm_height)?
                .check()?;
            let current = display
                .connection
                .randr_get_screen_resources_current(root)?
                .reply()?;
            let reply = display
                .connection
                .randr_set_crtc_config(
                    crtc,
                    0,
                    current.config_timestamp,
                    i16::try_from(x)?,
                    i16::try_from(y)?,
                    mode,
                    Rotation::ROTATE0,
                    &[output],
                )?
                .reply()
                .context("could not activate the temporary X11 output")?;
            if reply.status != SetConfig::SUCCESS {
                bail!(
                    "X11 could not activate temporary output {monitor_name}: {:?}",
                    reply.status
                );
            }
            display.connection.flush()?;
            Ok(())
        })();
        if let Err(error) = setup {
            if let Ok(cookie) = display.connection.randr_get_screen_resources_current(root)
                && let Ok(current) = cookie.reply()
                && let Ok(cookie) = display.connection.randr_set_crtc_config(
                    crtc,
                    0,
                    current.config_timestamp,
                    0,
                    0,
                    0,
                    Rotation::ROTATE0,
                    &[],
                )
            {
                let _ = cookie.reply();
            }
            if let Ok(cookie) = display.connection.randr_delete_output_mode(output, mode) {
                let _ = cookie.check();
            }
            if let Ok(cookie) = display.connection.randr_destroy_mode(mode) {
                let _ = cookie.check();
            }
            let _ = resize_to_active_crtcs(&display);
            return Err(error);
        }

        let session = Self {
            display,
            output,
            crtc,
            mode,
            monitor_name,
            stopped: false,
        };
        session.check()?;
        println!(
            "Created temporary X11 extended display {ordinal} ({}) at {}x{}+{}+{}.",
            session.monitor_name, width, height, x, y
        );
        Ok(session)
    }

    pub(crate) fn monitor_name(&self) -> &str {
        &self.monitor_name
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.stopped {
            bail!(
                "temporary X11 display {} is no longer active",
                self.monitor_name
            );
        }
        let resources = self
            .display
            .connection
            .randr_get_screen_resources_current(self.display.root())?
            .reply()?;
        let info = self
            .display
            .connection
            .randr_get_crtc_info(self.crtc, resources.config_timestamp)?
            .reply()?;
        if info.mode != self.mode || !info.outputs.contains(&self.output) {
            bail!(
                "temporary X11 display {} was reconfigured or disconnected",
                self.monitor_name
            );
        }
        self.display.select_monitor(Some(&self.monitor_name))?;
        Ok(())
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        let _layout = X11_LAYOUT
            .lock()
            .map_err(|_| anyhow!("X11 layout lock was poisoned"))?;
        let root = self.display.root();
        let resources = self
            .display
            .connection
            .randr_get_screen_resources_current(root)?
            .reply()?;
        let reply = self
            .display
            .connection
            .randr_set_crtc_config(
                self.crtc,
                0,
                resources.config_timestamp,
                0,
                0,
                0,
                Rotation::ROTATE0,
                &[],
            )?
            .reply()?;
        if reply.status != SetConfig::SUCCESS {
            bail!(
                "could not disable temporary X11 output {}",
                self.monitor_name
            );
        }
        self.display
            .connection
            .randr_delete_output_mode(self.output, self.mode)?
            .check()?;
        self.display
            .connection
            .randr_destroy_mode(self.mode)?
            .check()?;
        resize_to_active_crtcs(&self.display)?;
        self.display.connection.flush()?;
        println!(
            "Removed temporary X11 extended display {}.",
            self.monitor_name
        );
        Ok(())
    }
}

impl Drop for X11VirtualDisplay {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            log::warn!(
                "could not remove temporary X11 display {}: {error:#}",
                self.monitor_name
            );
        }
    }
}

fn pixels_to_mm(pixels: u16, reference_pixels: u16, reference_mm: u16) -> u32 {
    u32::from(pixels)
        .saturating_mul(u32::from(reference_mm))
        .div_ceil(u32::from(reference_pixels.max(1)))
        .max(1)
}

fn generated_mode(width: u16, height: u16, name_len: usize) -> Result<ModeInfo> {
    let hblank = (u32::from(width) / 5).max(160).div_ceil(8) * 8;
    let htotal = u32::from(width) + hblank;
    let hsync_start = u32::from(width) + hblank / 3;
    let hsync_end = hsync_start + hblank / 3;
    let vtotal = u32::from(height) + 28;
    let dot_clock = htotal
        .checked_mul(vtotal)
        .and_then(|v| v.checked_mul(60))
        .ok_or_else(|| anyhow!("extended display mode clock overflowed"))?;
    Ok(ModeInfo {
        id: 0,
        width,
        height,
        dot_clock,
        hsync_start: u16::try_from(hsync_start)?,
        hsync_end: u16::try_from(hsync_end)?,
        htotal: u16::try_from(htotal)?,
        hskew: 0,
        vsync_start: height
            .checked_add(3)
            .ok_or_else(|| anyhow!("vertical sync overflowed"))?,
        vsync_end: height
            .checked_add(8)
            .ok_or_else(|| anyhow!("vertical sync overflowed"))?,
        vtotal: u16::try_from(vtotal)?,
        name_len: u16::try_from(name_len)?,
        mode_flags: ModeFlag::HSYNC_NEGATIVE | ModeFlag::VSYNC_POSITIVE,
    })
}

fn resize_to_active_crtcs(display: &DisplayConnection) -> Result<()> {
    let root = display.root();
    let resources = display
        .connection
        .randr_get_screen_resources_current(root)?
        .reply()?;
    let mut width = 0_i32;
    let mut height = 0_i32;
    for crtc in resources.crtcs {
        let info = display
            .connection
            .randr_get_crtc_info(crtc, resources.config_timestamp)?
            .reply()?;
        if info.mode != 0 {
            width = width.max(i32::from(info.x) + i32::from(info.width));
            height = height.max(i32::from(info.y) + i32::from(info.height));
        }
    }
    let width = u16::try_from(width.max(1))?;
    let height = u16::try_from(height.max(1))?;
    let screen = display.screen();
    display
        .connection
        .randr_set_screen_size(
            root,
            width,
            height,
            pixels_to_mm(width, screen.width_in_pixels, screen.width_in_millimeters),
            pixels_to_mm(
                height,
                screen.height_in_pixels,
                screen.height_in_millimeters,
            ),
        )?
        .check()?;
    Ok(())
}

pub(crate) fn resolve_backend(preference: BackendPreference) -> Result<Backend> {
    match preference {
        BackendPreference::Portal => Ok(Backend::Portal),
        BackendPreference::X11 => {
            DisplayConnection::connect()?;
            Ok(Backend::X11)
        }
        BackendPreference::Auto => {
            let session_type = env::var("XDG_SESSION_TYPE").unwrap_or_default();
            let has_display = env::var_os("DISPLAY").is_some();
            if session_type.eq_ignore_ascii_case("x11") || has_display {
                DisplayConnection::connect().context(
                    "this looks like an X11 session, so automatic capture will not silently fall back to the portal",
                )?;
                Ok(Backend::X11)
            } else {
                Ok(Backend::Portal)
            }
        }
    }
}

pub(crate) fn list_monitors() -> Result<()> {
    let display = env::var("DISPLAY").unwrap_or_else(|_| "<unset>".to_owned());
    println!("X11 display {display} monitors:");
    for monitor in DisplayConnection::connect()?.monitors()? {
        let primary = if monitor.primary { " (primary)" } else { "" };
        println!("  {}: {}{}", monitor.name, monitor.geometry(), primary);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PixelLayout {
    format: RawPixelFormat,
    bits_per_pixel: u8,
    scanline_pad: u8,
}

impl PixelLayout {
    fn stride(self, width: u16) -> Result<usize> {
        let bits = usize::from(width)
            .checked_mul(usize::from(self.bits_per_pixel))
            .ok_or_else(|| anyhow!("X11 frame row size overflowed"))?;
        let pad = usize::from(self.scanline_pad);
        if pad == 0 || !pad.is_power_of_two() {
            bail!("X11 reported invalid scanline padding {pad}");
        }
        Ok(bits.div_ceil(pad) * pad / 8)
    }
}

fn pixel_layout(display: &DisplayConnection) -> Result<PixelLayout> {
    let setup = display.connection.setup();
    let screen = &setup.roots[display.screen_number];
    let pixmap = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == screen.root_depth)
        .ok_or_else(|| {
            anyhow!(
                "X11 has no pixmap format for root depth {}",
                screen.root_depth
            )
        })?;
    if pixmap.bits_per_pixel != 32 {
        bail!(
            "unsupported X11 root layout: depth {} uses {} bits per pixel (32 required)",
            screen.root_depth,
            pixmap.bits_per_pixel
        );
    }
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|depth| &depth.visuals)
        .find(|visual| visual.visual_id == screen.root_visual)
        .ok_or_else(|| anyhow!("X11 root visual {} was not described", screen.root_visual))?;
    let masks = (visual.red_mask, visual.green_mask, visual.blue_mask);
    let format = pixel_format(setup.image_byte_order, masks, pixmap.bits_per_pixel)?;
    Ok(PixelLayout {
        format,
        bits_per_pixel: pixmap.bits_per_pixel,
        scanline_pad: pixmap.scanline_pad,
    })
}

fn pixel_format(
    byte_order: ImageOrder,
    masks: (u32, u32, u32),
    bits_per_pixel: u8,
) -> Result<RawPixelFormat> {
    if bits_per_pixel != 32 {
        bail!("unsupported X11 pixel size {bits_per_pixel}; 32 bits per pixel are required");
    }
    Ok(match (byte_order, masks) {
        (ImageOrder::LSB_FIRST, (0x00ff0000, 0x0000ff00, 0x000000ff)) => RawPixelFormat::Bgrx,
        (ImageOrder::LSB_FIRST, (0x000000ff, 0x0000ff00, 0x00ff0000)) => RawPixelFormat::Rgbx,
        _ => bail!(
            "unsupported X11 root visual masks {:#010x}/{:#010x}/{:#010x} with image byte order {:?}",
            masks.0,
            masks.1,
            masks.2,
            byte_order
        ),
    })
}

struct ShmImage {
    segment: shm::Seg,
    address: NonNull<u8>,
    length: usize,
}

impl ShmImage {
    fn create(connection: &RustConnection, length: usize) -> Result<Option<Self>> {
        if env::var_os("CAST_X11_DISABLE_SHM").is_some() {
            return Ok(None);
        }
        if connection
            .extension_information(shm::X11_EXTENSION_NAME)
            .context("could not query the MIT-SHM extension")?
            .is_none()
        {
            return Ok(None);
        }
        let version = connection
            .shm_query_version()
            .context("could not query MIT-SHM version")?
            .reply()
            .context("could not read MIT-SHM version")?;
        if !supports_fd_shm(version.major_version, version.minor_version) {
            return Ok(None);
        }
        let length_u32 = u32::try_from(length).context("X11 frame is too large for MIT-SHM")?;
        let segment = connection.generate_id()?;
        let reply = connection
            .shm_create_segment(segment, length_u32, false)
            .context("could not create an MIT-SHM segment")?
            .reply()
            .context("the X11 server could not create an MIT-SHM segment")?;
        // SAFETY: The server returned a live descriptor for exactly `length`
        // bytes. MAP_SHARED keeps the mapping valid after the descriptor closes.
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                reply.shm_fd.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            if let Ok(cookie) = connection.shm_detach(segment) {
                let _ = cookie.check();
            }
            bail!("could not map the MIT-SHM frame buffer");
        }
        Ok(Some(Self {
            segment,
            address: NonNull::new(mapped.cast())
                .ok_or_else(|| anyhow!("MIT-SHM returned a null mapping"))?,
            length,
        }))
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: The mapping remains live for Self's lifetime and is only read
        // after the synchronous GetImage reply has completed.
        unsafe { std::slice::from_raw_parts(self.address.as_ptr(), self.length) }
    }
}

const fn supports_fd_shm(major: u16, minor: u16) -> bool {
    major > 1 || (major == 1 && minor >= 2)
}

impl Drop for ShmImage {
    fn drop(&mut self) {
        // SAFETY: address and length are the exact successful mmap result.
        unsafe { libc::munmap(self.address.as_ptr().cast(), self.length) };
    }
}

pub(crate) struct X11Capture {
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
    source_description: String,
}

impl X11Capture {
    pub(crate) fn start(
        monitor_name: Option<String>,
        fps: u32,
        sink: Arc<dyn FrameSink>,
        capture_epoch: CaptureEpoch,
    ) -> Result<Self> {
        if fps == 0 {
            bail!("X11 capture fps must be greater than zero");
        }
        let display = DisplayConnection::connect()?;
        let monitor = display.select_monitor(monitor_name.as_deref())?;
        pixel_layout(&display)?;
        let source_description = format!("X11 monitor {} ({})", monitor.name, monitor.geometry());
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let thread_stop = Arc::clone(&stop);
        let thread_failure = Arc::clone(&failure);
        let thread = thread::Builder::new()
            .name("cast-x11-video".to_owned())
            .spawn(move || {
                if let Err(error) =
                    run_capture(monitor.name, fps, sink, capture_epoch, &thread_stop)
                    && let Ok(mut failure) = thread_failure.lock()
                {
                    *failure = Some(format!("{error:#}"));
                }
            })
            .context("could not start the X11 capture thread")?;
        Ok(Self {
            stop,
            failure,
            thread: Some(thread),
            source_description,
        })
    }
}

impl CaptureBackend for X11Capture {
    fn source_description(&self) -> &str {
        &self.source_description
    }

    fn check(&self) -> Result<()> {
        if let Some(error) = self
            .failure
            .lock()
            .map_err(|_| anyhow!("X11 capture failure lock was poisoned"))?
            .as_ref()
        {
            bail!("X11 desktop capture failed: {error}");
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("X11 capture thread panicked"))?;
        }
        self.check()
    }
}

impl Drop for X11Capture {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            log::warn!("could not stop X11 desktop capture: {error:#}");
        }
    }
}

fn run_capture(
    monitor_name: String,
    fps: u32,
    sink: Arc<dyn FrameSink>,
    capture_epoch: CaptureEpoch,
    stop: &AtomicBool,
) -> Result<()> {
    let display = DisplayConnection::connect()?;
    let layout = pixel_layout(&display)?;
    let mut monitor = display.select_monitor(Some(&monitor_name))?;
    let mut stride = layout.stride(monitor.width)?;
    let mut length = frame_length(stride, monitor.height)?;
    let mut shm = match ShmImage::create(&display.connection, length) {
        Ok(segment) => segment,
        Err(error) => {
            log::warn!("MIT-SHM setup failed ({error:#}); using slower core X11 GetImage capture");
            None
        }
    };
    if shm.is_none() {
        log::warn!("MIT-SHM is unavailable; using slower core X11 GetImage capture");
    }
    let xfixes = display
        .connection
        .xfixes_query_version(5, 0)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some();
    if !xfixes {
        log::warn!("XFixes is unavailable; X11 cursor capture is disabled");
    }
    let interval = Duration::from_nanos(1_000_000_000_u64 / u64::from(fps));
    let mut next_frame = Instant::now();
    let mut geometry_check = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        if geometry_check.elapsed() >= Duration::from_secs(1) {
            let updated = display.select_monitor(Some(&monitor_name))?;
            if updated != monitor {
                monitor = updated;
                stride = layout.stride(monitor.width)?;
                length = frame_length(stride, monitor.height)?;
                if let Some(old) = shm.take() {
                    display.connection.shm_detach(old.segment)?.check()?;
                    drop(old);
                }
                shm = ShmImage::create(&display.connection, length).unwrap_or_else(|error| {
                    log::warn!("MIT-SHM resize failed ({error:#}); using core X11 GetImage");
                    None
                });
                log::info!("X11 monitor geometry changed to {}", monitor.geometry());
            }
            geometry_check = Instant::now();
        }
        let data = if let Some(segment) = &shm {
            display
                .connection
                .shm_get_image(
                    display.root(),
                    monitor.x,
                    monitor.y,
                    monitor.width,
                    monitor.height,
                    u32::MAX,
                    ImageFormat::Z_PIXMAP.into(),
                    segment.segment,
                    0,
                )?
                .reply()
                .context("MIT-SHM GetImage failed")?;
            segment.bytes().to_vec()
        } else {
            let reply = display
                .connection
                .get_image(
                    ImageFormat::Z_PIXMAP,
                    display.root(),
                    monitor.x,
                    monitor.y,
                    monitor.width,
                    monitor.height,
                    u32::MAX,
                )?
                .reply()
                .context("core X11 GetImage failed")?;
            if reply.data.len() != length {
                bail!(
                    "X11 returned {} frame bytes, expected {length}",
                    reply.data.len()
                );
            }
            reply.data
        };
        let cursor = xfixes
            .then(|| capture_cursor(&display, &monitor))
            .transpose()?;
        sink.submit(CapturedFrame {
            data,
            stride,
            width: u32::from(monitor.width),
            height: u32::from(monitor.height),
            format: layout.format,
            timestamp: capture_epoch.ticks(90_000),
            cursor,
        });
        next_frame += interval;
        thread::sleep(next_frame.saturating_duration_since(Instant::now()));
        if next_frame < Instant::now() {
            next_frame = Instant::now();
        }
    }
    if let Some(segment) = shm {
        display.connection.shm_detach(segment.segment)?.check()?;
    }
    Ok(())
}

fn frame_length(stride: usize, height: u16) -> Result<usize> {
    let length = stride
        .checked_mul(usize::from(height))
        .ok_or_else(|| anyhow!("X11 frame size overflowed"))?;
    if length == 0 || length > MAX_FRAME_BYTES {
        bail!("X11 frame size {length} is outside the supported range");
    }
    Ok(length)
}

fn capture_cursor(display: &DisplayConnection, monitor: &Monitor) -> Result<CursorImage> {
    let reply = display
        .connection
        .xfixes_get_cursor_image()?
        .reply()
        .context("XFixes cursor capture failed")?;
    let mut pixels = Vec::with_capacity(reply.cursor_image.len() * 4);
    for pixel in reply.cursor_image {
        pixels.extend_from_slice(&[
            pixel as u8,
            (pixel >> 8) as u8,
            (pixel >> 16) as u8,
            (pixel >> 24) as u8,
        ]);
    }
    Ok(CursorImage {
        position: (
            i32::from(reply.x) - i32::from(monitor.x),
            i32::from(reply.y) - i32::from(monitor.y),
        ),
        hotspot: (i32::from(reply.xhot), i32::from(reply.yhot)),
        width: u32::from(reply.width),
        height: u32::from(reply.height),
        stride: usize::from(reply.width) * 4,
        format: CursorPixelFormat::Bgra,
        pixels,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use super::*;
    use x11rb::{
        COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT,
        protocol::xproto::{ChangeWindowAttributesAux, CreateWindowAux, WindowClass},
    };

    #[test]
    fn monitor_geometry_is_xrandr_compatible() {
        let monitor = Monitor {
            name: "DP-1".to_owned(),
            x: -1920,
            y: 40,
            width: 1920,
            height: 1080,
            primary: false,
        };
        assert_eq!(monitor.geometry(), "1920x1080-1920+40");
    }

    #[test]
    fn generated_virtual_mode_has_valid_60_hz_timings() {
        let mode = generated_mode(1280, 720, 12).unwrap();
        assert_eq!((mode.width, mode.height, mode.name_len), (1280, 720, 12));
        assert!(mode.hsync_start > mode.width);
        assert!(mode.hsync_end > mode.hsync_start);
        assert!(mode.htotal > mode.hsync_end);
        assert!(mode.vsync_start > mode.height);
        assert!(mode.vsync_end > mode.vsync_start);
        assert!(mode.vtotal > mode.vsync_end);
        assert_eq!(
            mode.dot_clock,
            u32::from(mode.htotal) * u32::from(mode.vtotal) * 60
        );
    }

    #[test]
    fn physical_size_preserves_reference_dpi() {
        assert_eq!(pixels_to_mm(1920, 1920, 293), 293);
        assert_eq!(pixels_to_mm(3200, 1920, 293), 489);
        assert_eq!(pixels_to_mm(1280, 0, 0), 1);
    }

    #[test]
    fn maps_common_little_endian_x11_visuals() {
        assert_eq!(
            pixel_format(
                ImageOrder::LSB_FIRST,
                (0x00ff0000, 0x0000ff00, 0x000000ff),
                32
            )
            .unwrap(),
            RawPixelFormat::Bgrx
        );
        assert_eq!(
            pixel_format(
                ImageOrder::LSB_FIRST,
                (0x000000ff, 0x0000ff00, 0x00ff0000),
                32
            )
            .unwrap(),
            RawPixelFormat::Rgbx
        );
        assert!(pixel_format(ImageOrder::MSB_FIRST, (0, 0, 0), 32).is_err());
        assert!(pixel_format(ImageOrder::LSB_FIRST, (0, 0, 0), 24).is_err());
    }

    struct CountingSink {
        count: AtomicUsize,
        first_checksum: AtomicU64,
        changed: AtomicBool,
    }

    impl FrameSink for CountingSink {
        fn submit(&self, frame: CapturedFrame) {
            assert_eq!(frame.data.len(), frame.stride * frame.height as usize);
            let checksum = frame
                .data
                .chunks(frame.stride)
                .take(64)
                .flat_map(|row| row.iter().take(64 * 4))
                .fold(0_u64, |sum, byte| {
                    sum.wrapping_mul(16_777_619) ^ u64::from(*byte)
                });
            let first = self.first_checksum.load(Ordering::Relaxed);
            if first == 0 {
                self.first_checksum
                    .store(checksum.max(1), Ordering::Relaxed);
            } else if checksum != first {
                self.changed.store(true, Ordering::Relaxed);
            }
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn captures_live_x11_frames_when_integration_test_is_enabled() {
        if env::var_os("CAST_X11_INTEGRATION_TEST").is_none() {
            return;
        }
        let display = DisplayConnection::connect().unwrap();
        let monitor = display.select_monitor(None).unwrap();
        let window = display.connection.generate_id().unwrap();
        display
            .connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                display.root(),
                monitor.x,
                monitor.y,
                64,
                64,
                0,
                WindowClass::INPUT_OUTPUT,
                COPY_FROM_PARENT,
                &CreateWindowAux::new()
                    .override_redirect(1)
                    .background_pixel(0x00ff0000),
            )
            .unwrap()
            .check()
            .unwrap();
        display
            .connection
            .map_window(window)
            .unwrap()
            .check()
            .unwrap();
        display.connection.flush().unwrap();
        let sink = Arc::new(CountingSink {
            count: AtomicUsize::new(0),
            first_checksum: AtomicU64::new(0),
            changed: AtomicBool::new(false),
        });
        let erased: Arc<dyn FrameSink> = sink.clone();
        let mut capture = X11Capture::start(None, 10, erased, CaptureEpoch::new()).unwrap();
        thread::sleep(Duration::from_millis(250));
        display
            .connection
            .change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new().background_pixel(0x000000ff),
            )
            .unwrap()
            .check()
            .unwrap();
        display
            .connection
            .clear_area(false, window, 0, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
        display.connection.flush().unwrap();
        thread::sleep(Duration::from_millis(250));
        capture.check().unwrap();
        capture.stop().unwrap();
        display
            .connection
            .destroy_window(window)
            .unwrap()
            .check()
            .unwrap();
        assert!(sink.count.load(Ordering::Relaxed) >= 4);
        assert!(sink.changed.load(Ordering::Relaxed));
    }

    #[test]
    fn mit_shm_fd_support_requires_protocol_1_2() {
        assert!(!supports_fd_shm(0, 0));
        assert!(!supports_fd_shm(1, 1));
        assert!(supports_fd_shm(1, 2));
        assert!(supports_fd_shm(2, 0));
    }
}
