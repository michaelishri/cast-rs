use std::{
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
        randr::ConnectionExt as _,
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
