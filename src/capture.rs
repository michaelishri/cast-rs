use std::{
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use screencapturekit::prelude::*;
use videotoolbox::prelude::*;

pub struct CaptureOptions {
    pub display_id: Option<u32>,
    pub duration: Duration,
    pub fps: u32,
    pub bitrate: i32,
    pub output: PathBuf,
}

pub fn list_displays() -> Result<()> {
    let content = SCShareableContent::get().context(
        "could not enumerate displays; grant Screen Recording permission in System Settings",
    )?;
    let displays = content.displays();
    if displays.is_empty() {
        println!("No displays found.");
        return Ok(());
    }

    println!("ID\tWIDTH\tHEIGHT\tORIGIN");
    for display in displays {
        let frame = display.frame();
        println!(
            "{}\t{}\t{}\t{},{}",
            display.display_id(),
            display.width(),
            display.height(),
            frame.origin.x,
            frame.origin.y
        );
    }
    Ok(())
}

pub fn capture(options: CaptureOptions) -> Result<()> {
    if options.bitrate <= 0 {
        bail!("bitrate must be greater than zero");
    }

    let content = SCShareableContent::get().context(
        "could not enumerate displays; grant Screen Recording permission in System Settings",
    )?;
    let displays = content.displays();
    let display = match options.display_id {
        Some(id) => displays
            .iter()
            .find(|display| display.display_id() == id)
            .ok_or_else(|| anyhow!("display {id} was not found"))?,
        None => displays
            .first()
            .ok_or_else(|| anyhow!("no displays found"))?,
    };

    let width = even(display.width());
    let height = even(display.height());
    let encoder = CompressionSession::builder(width as i32, height as i32, Codec::H264)
        .with_real_time(true)
        .with_allow_frame_reordering(false)
        .with_average_bit_rate(options.bitrate)
        .with_expected_frame_rate(options.fps as f64)
        .with_max_keyframe_interval(options.fps as i32)
        .build()
        .context("could not create the VideoToolbox H.264 encoder")?;

    let writer = BufWriter::new(
        File::create(&options.output)
            .with_context(|| format!("could not create {}", options.output.display()))?,
    );
    let state = Arc::new(CaptureState {
        encoder,
        writer: Mutex::new(writer),
        frames: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        first_error: Mutex::new(None),
        fps: options.fps as i32,
    });

    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build();
    let frame_interval = CMTime::new(1, options.fps as i32);
    let config = SCStreamConfiguration::new()
        .with_width(width)
        .with_height(height)
        .with_pixel_format(PixelFormat::YCbCr_420v)
        .with_shows_cursor(true)
        .with_queue_depth(4)
        .with_minimum_frame_interval(&frame_interval);

    let mut stream = SCStream::new(&filter, &config);
    stream.add_output_handler(
        FrameHandler {
            state: Arc::clone(&state),
        },
        SCStreamOutputType::Screen,
    );

    println!(
        "Capturing display {} at {}x{}, {} fps for {}s...",
        display.display_id(),
        width,
        height,
        options.fps,
        options.duration.as_secs()
    );
    stream
        .start_capture()
        .context("could not start screen capture")?;
    thread::sleep(options.duration);
    stream
        .stop_capture()
        .context("could not stop screen capture")?;

    state
        .writer
        .lock()
        .map_err(|_| anyhow!("output writer lock was poisoned"))?
        .flush()
        .context("could not flush encoded output")?;
    if let Some(error) = state
        .first_error
        .lock()
        .map_err(|_| anyhow!("capture error lock was poisoned"))?
        .take()
    {
        bail!("capture failed: {error}");
    }

    println!(
        "Encoded {} frames ({} bytes) to {}",
        state.frames.load(Ordering::Relaxed),
        state.bytes.load(Ordering::Relaxed),
        options.output.display()
    );
    Ok(())
}

struct CaptureState {
    encoder: CompressionSession,
    writer: Mutex<BufWriter<File>>,
    frames: AtomicU64,
    bytes: AtomicU64,
    first_error: Mutex<Option<String>>,
    fps: i32,
}

struct FrameHandler {
    state: Arc<CaptureState>,
}

impl SCStreamOutputTrait for FrameHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, output_type: SCStreamOutputType) {
        if output_type != SCStreamOutputType::Screen
            || sample
                .frame_status()
                .is_some_and(|status| !status.has_content())
        {
            return;
        }

        let result = (|| -> Result<()> {
            let pixel_buffer = sample
                .image_buffer()
                .ok_or_else(|| anyhow!("screen frame had no pixel buffer"))?;
            let surface = pixel_buffer
                .io_surface()
                .ok_or_else(|| anyhow!("screen frame was not IOSurface-backed"))?;
            let index = self.state.frames.load(Ordering::Relaxed) as i64;
            let encoded = self
                .state
                .encoder
                .encode(&surface, (index, self.state.fps))
                .context("VideoToolbox could not encode a frame")?;
            self.state
                .writer
                .lock()
                .map_err(|_| anyhow!("output writer lock was poisoned"))?
                .write_all(&encoded.data)
                .context("could not write encoded frame")?;
            self.state
                .bytes
                .fetch_add(encoded.data.len() as u64, Ordering::Relaxed);
            self.state.frames.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })();

        if let Err(error) = result
            && let Ok(mut first_error) = self.state.first_error.lock()
            && first_error.is_none()
        {
            *first_error = Some(format!("{error:#}"));
        }
    }
}

const fn even(value: u32) -> u32 {
    value - (value % 2)
}

#[cfg(test)]
mod tests {
    use super::even;

    #[test]
    fn encoder_dimensions_are_even() {
        assert_eq!(even(1920), 1920);
        assert_eq!(even(1921), 1920);
    }
}
