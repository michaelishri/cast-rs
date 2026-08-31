use std::{
    convert::TryFrom,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::linux_encoder::RawPixelFormat;
use crate::{
    linux_x11::{
        Backend, BackendPreference, DisplayConnection, X11Capture, X11VirtualDisplay,
        resolve_backend,
    },
    portal::{PipeWireCapture, PortalSelection, PortalSourceKind},
};

/// Monotonic epoch shared by every media stream in one Linux desktop session.
#[derive(Clone)]
pub(crate) struct CaptureEpoch {
    started: Arc<Instant>,
}

impl CaptureEpoch {
    pub(crate) fn new() -> Self {
        Self {
            started: Arc::new(Instant::now()),
        }
    }

    pub(crate) fn ticks(&self, timescale: u64) -> u64 {
        duration_ticks(self.started.elapsed(), timescale)
    }

    #[cfg(test)]
    fn shares_origin_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.started, &other.started)
    }
}

pub(crate) fn duration_ticks(elapsed: Duration, timescale: u64) -> u64 {
    u64::try_from(elapsed.as_nanos())
        .unwrap_or(u64::MAX)
        .saturating_mul(timescale)
        / 1_000_000_000
}

#[derive(Clone, Debug)]
pub(crate) struct CursorImage {
    pub(crate) position: (i32, i32),
    pub(crate) hotspot: (i32, i32),
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: usize,
    pub(crate) format: CursorPixelFormat,
    pub(crate) pixels: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorPixelFormat {
    Rgbx,
    Bgrx,
    Xrgb,
    Xbgr,
    Rgba,
    Bgra,
    Argb,
    Abgr,
}

impl CursorPixelFormat {
    pub(crate) fn rgba(self, pixel: &[u8]) -> (u8, u8, u8, u8) {
        match self {
            Self::Rgbx => (pixel[0], pixel[1], pixel[2], 255),
            Self::Bgrx => (pixel[2], pixel[1], pixel[0], 255),
            Self::Xrgb => (pixel[1], pixel[2], pixel[3], 255),
            Self::Xbgr => (pixel[3], pixel[2], pixel[1], 255),
            Self::Rgba => (pixel[0], pixel[1], pixel[2], pixel[3]),
            Self::Bgra => (pixel[2], pixel[1], pixel[0], pixel[3]),
            Self::Argb => (pixel[1], pixel[2], pixel[3], pixel[0]),
            Self::Abgr => (pixel[3], pixel[2], pixel[1], pixel[0]),
        }
    }
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

pub(crate) fn validate_source_options(
    preference: BackendPreference,
    display_name: Option<&str>,
    force_chooser: bool,
    extend: bool,
) -> Result<Backend> {
    let backend = resolve_backend(preference)?;
    match backend {
        Backend::X11 => {
            if force_chooser {
                anyhow::bail!("--select-source is available only with --backend portal");
            }
            if !extend {
                DisplayConnection::connect()?.select_monitor(display_name)?;
            }
        }
        Backend::Portal if display_name.is_some() => {
            anyhow::bail!("--display is available only with --backend x11");
        }
        Backend::Portal => {}
    }
    Ok(backend)
}

pub(crate) enum ExtendedDisplaySession {
    Portal(PortalSelection),
    X11(Box<X11VirtualDisplay>),
}

impl ExtendedDisplaySession {
    pub(crate) fn start(
        preference: BackendPreference,
        width: u32,
        height: u32,
        ordinal: u32,
    ) -> Result<Self> {
        match resolve_backend(preference)? {
            Backend::X11 => Ok(Self::X11(Box::new(X11VirtualDisplay::start(
                width, height, ordinal,
            )?))),
            Backend::Portal => Ok(Self::Portal(crate::portal::select(
                PortalSourceKind::Virtual,
                true,
            )?)),
        }
    }

    pub(crate) fn description(&self) -> String {
        match self {
            Self::Portal(selection) => format!(
                "portal virtual source (PipeWire node {})",
                selection.node_id()
            ),
            Self::X11(session) => format!("X11 output {}", session.monitor_name()),
        }
    }

    pub(crate) fn check(&mut self) -> Result<()> {
        match self {
            Self::Portal(selection) => selection.check(),
            Self::X11(session) => session.check(),
        }
    }
}

pub(crate) fn start_desktop_capture(
    extended_display: Option<ExtendedDisplaySession>,
    preference: BackendPreference,
    display_name: Option<String>,
    force_chooser: bool,
    fps: u32,
    sink: Arc<dyn FrameSink>,
    capture_epoch: CaptureEpoch,
) -> Result<RunningCapture> {
    if let Some(display) = extended_display {
        return match display {
            ExtendedDisplaySession::Portal(selection) => Ok(RunningCapture::new(
                PipeWireCapture::start_at(selection, sink, capture_epoch)?,
            )),
            ExtendedDisplaySession::X11(session) => {
                let monitor = session.monitor_name().to_owned();
                let deadline = Instant::now() + Duration::from_secs(2);
                let capture = loop {
                    session.check()?;
                    match X11Capture::start(
                        Some(monitor.clone()),
                        fps,
                        Arc::clone(&sink),
                        capture_epoch.clone(),
                    ) {
                        Ok(capture) => break capture,
                        Err(error) if Instant::now() < deadline => {
                            log::debug!(
                                "temporary X11 monitor {monitor} is not capture-ready yet: {error:#}"
                            );
                            thread::sleep(Duration::from_millis(50));
                        }
                        Err(error) => return Err(error),
                    }
                };
                Ok(RunningCapture::with_virtual_display(capture, *session))
            }
        };
    }
    match validate_source_options(preference, display_name.as_deref(), force_chooser, false)? {
        Backend::X11 => Ok(RunningCapture::new(X11Capture::start(
            display_name,
            fps,
            sink,
            capture_epoch,
        )?)),
        Backend::Portal => {
            let selection = crate::portal::select(PortalSourceKind::Normal, force_chooser)?;
            Ok(RunningCapture::new(PipeWireCapture::start_at(
                selection,
                sink,
                capture_epoch,
            )?))
        }
    }
}

pub(crate) trait CaptureBackend {
    fn source_description(&self) -> &str;
    fn check(&self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
}

pub(crate) struct RunningCapture {
    backend: Box<dyn CaptureBackend>,
    virtual_display: Option<Box<X11VirtualDisplay>>,
    stopped: bool,
}

impl RunningCapture {
    pub(crate) fn new(backend: impl CaptureBackend + 'static) -> Self {
        Self {
            backend: Box::new(backend),
            virtual_display: None,
            stopped: false,
        }
    }

    fn with_virtual_display(
        backend: impl CaptureBackend + 'static,
        virtual_display: X11VirtualDisplay,
    ) -> Self {
        Self {
            backend: Box::new(backend),
            virtual_display: Some(Box::new(virtual_display)),
            stopped: false,
        }
    }

    pub(crate) fn source_description(&self) -> &str {
        self.backend.source_description()
    }

    pub(crate) fn check(&self) -> Result<()> {
        self.backend.check()
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        self.backend.stop()?;
        if let Some(mut display) = self.virtual_display.take() {
            display.stop()?;
        }
        Ok(())
    }
}

impl Drop for RunningCapture {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            log::warn!(
                "could not stop {} capture: {error:#}",
                self.source_description()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use anyhow::{Result, bail};

    use super::*;

    struct TestCapture {
        stopped: Arc<AtomicBool>,
        fail: bool,
    }

    impl CaptureBackend for TestCapture {
        fn source_description(&self) -> &str {
            "test source"
        }

        fn check(&self) -> Result<()> {
            if self.fail {
                bail!("test failure")
            }
            Ok(())
        }

        fn stop(&mut self) -> Result<()> {
            self.stopped.store(true, Ordering::SeqCst);
            self.check()
        }
    }

    #[test]
    fn one_epoch_maps_audio_video_and_extended_sources_without_rebasing() {
        assert_eq!(duration_ticks(Duration::from_millis(250), 90_000), 22_500);
        assert_eq!(duration_ticks(Duration::from_millis(250), 48_000), 12_000);
        assert_eq!(duration_ticks(Duration::from_secs(2), 90_000), 180_000);

        let session = CaptureEpoch::new();
        let extended_source = session.clone();
        assert!(session.shares_origin_with(&extended_source));
        assert!(!session.shares_origin_with(&CaptureEpoch::new()));
    }

    #[test]
    fn running_capture_exposes_description_and_propagates_failures() {
        let stopped = Arc::new(AtomicBool::new(false));
        let mut capture = RunningCapture::new(TestCapture {
            stopped: Arc::clone(&stopped),
            fail: true,
        });
        assert_eq!(capture.source_description(), "test source");
        assert_eq!(capture.check().unwrap_err().to_string(), "test failure");
        assert_eq!(capture.stop().unwrap_err().to_string(), "test failure");
        assert!(stopped.load(Ordering::SeqCst));
    }

    #[test]
    fn running_capture_stops_on_drop() {
        let stopped = Arc::new(AtomicBool::new(false));
        {
            let _capture = RunningCapture::new(TestCapture {
                stopped: Arc::clone(&stopped),
                fail: false,
            });
        }
        assert!(stopped.load(Ordering::SeqCst));
    }
}
