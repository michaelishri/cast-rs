use std::{
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    desktop::{LatestFrameBackend, LatestFrameObserver, LatestFrameSubmitter, LatestFrameWorker},
    linux_capture::{CapturedFrame, CursorImage, FrameSink, RunningCapture},
    linux_encoder::{EncodingPriority, LinuxVideoEncoder, RawVideoFrame},
    media::H264Provider,
    portal::{PipeWireCapture, PortalSourceKind},
};

pub(crate) struct CaptureOptions {
    pub(crate) backend: crate::linux_x11::BackendPreference,
    pub(crate) display: Option<String>,
    pub(crate) force_chooser: bool,
    pub(crate) duration: Duration,
    pub(crate) fps: u32,
    pub(crate) bitrate: u32,
    pub(crate) provider: H264Provider,
    pub(crate) output: PathBuf,
}

pub(crate) fn capture(options: CaptureOptions) -> Result<()> {
    let selection = crate::portal::select(PortalSourceKind::Normal, options.force_chooser)?;
    let writer = BufWriter::new(
        File::create(&options.output)
            .with_context(|| format!("could not create {}", options.output.display()))?,
    );
    let stats = Arc::new(CaptureStats::default());
    let failure = Arc::new(Mutex::new(None));
    let backend = DiagnosticEncoder {
        encoder: None,
        provider: options.provider,
        fps: options.fps,
        bitrate: options.bitrate,
        writer,
        stats: Arc::clone(&stats),
    };
    let observer: Arc<dyn LatestFrameObserver> = stats.clone();
    let (submitter, mut worker) = LatestFrameWorker::start(
        backend,
        Some(Duration::from_millis(
            1_000_u64.div_ceil(u64::from(options.fps)),
        )),
        Arc::clone(&failure),
        observer,
        "cast-linux-capture-encoder",
    )?;
    let sink: Arc<dyn FrameSink> = Arc::new(PortalFrameSubmitter { submitter });
    let mut capture = RunningCapture::new(PipeWireCapture::start(selection, sink)?);
    println!(
        "Capturing {} at up to {} fps for {}s...",
        capture.source_description(),
        options.fps,
        options.duration.as_secs()
    );
    let started = Instant::now();
    while started.elapsed() < options.duration {
        capture.check()?;
        if let Some(error) = failure
            .lock()
            .map_err(|_| anyhow!("capture failure lock was poisoned"))?
            .as_ref()
        {
            bail!("Linux diagnostic encoder failed: {error}");
        }
        thread::sleep(Duration::from_millis(50));
    }
    capture.stop()?;
    worker.stop()?;
    if let Some(error) = failure
        .lock()
        .map_err(|_| anyhow!("capture failure lock was poisoned"))?
        .take()
    {
        bail!("Linux diagnostic encoder failed: {error}");
    }
    println!(
        "Encoded {} frames ({} bytes; {} replaced before encoding) to {}",
        stats.encoded.load(Ordering::Relaxed),
        stats.bytes.load(Ordering::Relaxed),
        stats.replaced.load(Ordering::Relaxed),
        options.output.display()
    );
    Ok(())
}

struct PortalFrameSubmitter {
    submitter: LatestFrameSubmitter<CapturedFrame>,
}

impl FrameSink for PortalFrameSubmitter {
    fn submit(&self, frame: CapturedFrame) {
        if let Err(error) = self.submitter.submit(frame) {
            log::debug!("PipeWire frame arrived after the encoder stopped: {error}");
        }
    }
}

#[derive(Default)]
struct CaptureStats {
    submitted: AtomicU64,
    replaced: AtomicU64,
    expired: AtomicU64,
    encoded: AtomicU64,
    bytes: AtomicU64,
}

impl LatestFrameObserver for CaptureStats {
    fn submitted(&self) {
        self.submitted.fetch_add(1, Ordering::Relaxed);
    }

    fn replaced(&self) {
        self.replaced.fetch_add(1, Ordering::Relaxed);
    }

    fn expired(&self) {
        self.expired.fetch_add(1, Ordering::Relaxed);
    }
}

struct DiagnosticEncoder {
    encoder: Option<LinuxVideoEncoder>,
    provider: H264Provider,
    fps: u32,
    bitrate: u32,
    writer: BufWriter<File>,
    stats: Arc<CaptureStats>,
}

// The FFmpeg scaler wrapper is conservatively !Send. This backend is moved to
// its dedicated worker before the encoder is created, then remains exclusively
// owned and used by that thread until it is dropped there.
unsafe impl Send for DiagnosticEncoder {}

impl LatestFrameBackend for DiagnosticEncoder {
    type Frame = CapturedFrame;

    fn has_reference_frame(&self) -> bool {
        self.encoder.is_some()
    }

    fn failure_context(&self) -> &'static str {
        "Linux diagnostic H.264 encoder failed"
    }

    fn process_frame(&mut self, mut frame: Self::Frame, _queue_wait_micros: u64) -> Result<()> {
        if let Some(cursor) = frame.cursor.take() {
            composite_cursor(&mut frame, &cursor);
        }
        let width = even(frame.width);
        let height = even(frame.height);
        if width == 0 || height == 0 {
            bail!("portal returned an empty video frame");
        }
        let rebuild = self.encoder.as_ref().is_none_or(|encoder| {
            encoder.dimensions() != (width, height) || encoder.input_format() != frame.format
        });
        if rebuild {
            log::debug!(
                "opening Linux encoder for portal format {:?} at {width}x{height}",
                frame.format
            );
            self.encoder = Some(LinuxVideoEncoder::new(
                self.provider,
                width,
                height,
                self.fps,
                self.bitrate,
                frame.format,
                EncodingPriority::Speed,
            )?);
        }
        let packets = self
            .encoder
            .as_mut()
            .expect("encoder was initialized")
            .encode(RawVideoFrame {
                data: &frame.data,
                stride: frame.stride,
                width,
                height,
                format: frame.format,
                timestamp: frame.timestamp,
            })?;
        for packet in packets {
            self.writer.write_all(&packet.data)?;
            self.stats
                .bytes
                .fetch_add(packet.data.len() as u64, Ordering::Relaxed);
            self.stats.encoded.fetch_add(1, Ordering::Relaxed);
        }
        self.writer
            .flush()
            .context("could not flush diagnostic H.264 output")
    }
}

pub(crate) fn composite_cursor(frame: &mut CapturedFrame, cursor: &CursorImage) {
    if cursor.stride < cursor.width as usize * 4 {
        return;
    }
    let left = cursor.position.0 - cursor.hotspot.0;
    let top = cursor.position.1 - cursor.hotspot.1;
    for cursor_y in 0..cursor.height as i32 {
        let target_y = top + cursor_y;
        if target_y < 0 || target_y >= frame.height as i32 {
            continue;
        }
        for cursor_x in 0..cursor.width as i32 {
            let target_x = left + cursor_x;
            if target_x < 0 || target_x >= frame.width as i32 {
                continue;
            }
            let source = cursor_y as usize * cursor.stride + cursor_x as usize * 4;
            let target = target_y as usize * frame.stride + target_x as usize * 4;
            if source + 4 <= cursor.pixels.len() && target + 4 <= frame.data.len() {
                let (red, green, blue, alpha) =
                    cursor.format.rgba(&cursor.pixels[source..source + 4]);
                let foreground = match frame.format {
                    crate::linux_encoder::RawPixelFormat::Bgra
                    | crate::linux_encoder::RawPixelFormat::Bgrx => [blue, green, red],
                    crate::linux_encoder::RawPixelFormat::Rgba
                    | crate::linux_encoder::RawPixelFormat::Rgbx => [red, green, blue],
                };
                let alpha = u16::from(alpha);
                for (channel, foreground) in foreground.into_iter().enumerate() {
                    let foreground = u16::from(foreground);
                    let background = u16::from(frame.data[target + channel]);
                    frame.data[target + channel] =
                        ((foreground * alpha + background * (255 - alpha)) / 255) as u8;
                }
            }
        }
    }
}

const fn even(value: u32) -> u32 {
    value - value % 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{linux_capture::CursorPixelFormat, linux_encoder::RawPixelFormat};

    #[test]
    fn cursor_metadata_is_composited_and_clipped() {
        let mut frame = CapturedFrame {
            data: vec![0; 4 * 4 * 4],
            stride: 16,
            width: 4,
            height: 4,
            format: RawPixelFormat::Bgrx,
            timestamp: 0,
            cursor: None,
        };
        let cursor = CursorImage {
            position: (0, 0),
            hotspot: (1, 1),
            width: 2,
            height: 2,
            stride: 8,
            format: CursorPixelFormat::Bgra,
            pixels: vec![1, 1, 1, 255, 1, 1, 1, 255, 1, 1, 1, 255, 1, 1, 1, 255],
        };
        composite_cursor(&mut frame, &cursor);
        assert_eq!(&frame.data[..4], &[1, 1, 1, 0]);
        assert!(frame.data[4..].iter().all(|value| *value == 0));
    }

    #[test]
    fn cursor_color_layout_and_alpha_are_converted_to_the_frame_layout() {
        let mut frame = CapturedFrame {
            data: vec![10, 20, 30, 0],
            stride: 4,
            width: 1,
            height: 1,
            format: RawPixelFormat::Rgbx,
            timestamp: 0,
            cursor: None,
        };
        let cursor = CursorImage {
            position: (0, 0),
            hotspot: (0, 0),
            width: 1,
            height: 1,
            stride: 4,
            format: CursorPixelFormat::Argb,
            pixels: vec![128, 110, 120, 130],
        };
        composite_cursor(&mut frame, &cursor);
        assert_eq!(frame.data, [60, 70, 80, 0]);
    }
}
