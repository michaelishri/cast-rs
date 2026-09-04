use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use ffmpeg::format::Pixel;
use ffmpeg::packet::traits::Mut as _;
use ffmpeg::software::resampling;
use ffmpeg::software::scaling;
use ffmpeg_next as ffmpeg;

use super::clock::{Clock, SampleRing, VolumeState};
use super::fetch::RangeReader;

/// Where the decoder sources media from.
#[derive(Clone, Debug)]
pub enum Source {
    #[allow(dead_code)] // used by the planned loopback integration tests
    LocalFile(PathBuf),
    Origin(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Playing,
    Paused,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // SetVolume arrives only from sender commands
pub enum Command {
    Play,
    Pause,
    Seek(f64),
    Stop,
    SetVolume { level: f64, muted: bool },
}

#[derive(Clone, Debug)]
pub enum Event {
    Opened {
        #[allow(dead_code)] // surfaced to the window title later
        title: String,
        duration: Option<f64>,
    },
    State(PlaybackState),
    Ended,
    Failed(String),
}

/// A decoded, presentable video frame with pixels pre-packed for `softbuffer`
/// (little-endian 0x00BBGGRR).
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
    pub pts_secs: f64,
}

/// The latest presentable frame, shared between the decode thread and the
/// window renderer.
pub type FrameSlot = Arc<std::sync::Mutex<Option<VideoFrame>>>;

/// Commands and events for one running playback, polled by the receiver core.
pub struct Handle {
    commands: std::sync::mpsc::Sender<Command>,
    events: std::sync::mpsc::Receiver<Event>,
}

impl Handle {
    pub fn send(&self, command: Command) {
        self.commands.send(command).ok();
    }

    /// Requests shutdown of the playback thread.
    pub fn stop(&self) {
        self.send(Command::Stop);
    }

    pub fn try_event(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }
}

/// Shared state between the decode loop, the audio callback, and the window.
#[allow(dead_code)] // ring/slot/stream are held alive for the playing session
pub struct Session {
    pub clock: Clock,
    pub ring: Arc<SampleRing>,
    pub volume: Arc<VolumeState>,
    pub slot: FrameSlot,
    /// Alive for as long as the session plays; keeps the audio stream open.
    stream: Option<cpal::Stream>,
}

const PRESENT_AHEAD_SECS: f64 = 0.08;
const MAX_PRESENT_WAIT_STEPS: u32 = 50;
const RING_HIGH_WATER: usize = RING_CAPACITY * 3 / 4;
const RING_CAPACITY: usize = 1 << 19;
const DRAIN_POLL: Duration = Duration::from_millis(5);
const AVIO_BUFFER_SIZE: usize = 64 * 1024;

/// Spawns a decode thread. Output starts playing immediately when
/// `autoplay`, otherwise it stays paused until a Play command.
pub fn spawn(
    source: Source,
    title: String,
    start_at: f64,
    autoplay: bool,
    window_enabled: bool,
    slot: FrameSlot,
) -> (Handle, Session) {
    let (commands, command_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();

    // Pick the master clock from the audio device, then open the stream.
    let ring = Arc::new(SampleRing::new(RING_CAPACITY));
    let output_device = probe_output();
    let (clock, stream) = match &output_device {
        Some((rate, channels, device)) => {
            let clock = Clock::sample_clock(*rate);
            match build_stream(device, *channels, *rate, Arc::clone(&ring), clock.clone()) {
                Ok(stream) => (clock, Some(stream)),
                Err(error) => {
                    log::debug!("audio output unavailable: {error}");
                    (Clock::wall_clock(), None)
                }
            }
        }
        None => {
            log::info!("no audio output device found; playback timing falls back to a wall clock");
            (Clock::wall_clock(), None)
        }
    };
    let volume = Arc::new(VolumeState::new(1.0, false));
    let (device_channels, device_sample_rate) = match &output_device {
        Some((rate, channels, _)) => (usize::from(*channels), *rate),
        None => (2, 48_000),
    };

    let session = Session {
        clock: clock.clone(),
        ring: Arc::clone(&ring),
        volume: volume.clone(),
        slot: Arc::clone(&slot),
        stream,
    };

    let spawn_result = std::thread::Builder::new()
        .name("cast-receiver-player".to_owned())
        .spawn({
            let event_tx = event_tx.clone();
            move || {
                let job = DecodeJob {
                    source,
                    title,
                    start_at,
                    autoplay,
                    window_enabled,
                    device_channels,
                    device_sample_rate,
                    slot,
                    commands: command_rx,
                    events: event_tx,
                    clock,
                    ring,
                    volume,
                };
                if let Err(error) = run_decode(job) {
                    log::debug!("receiver playback failed: {error:#}");
                }
            }
        });
    if let Err(error) = spawn_result {
        let _ = event_tx.send(Event::Failed(format!(
            "could not start the playback thread: {error}"
        )));
    }
    (
        Handle {
            commands,
            events: event_rx,
        },
        session,
    )
}

fn probe_output() -> Option<(u32, u16, cpal::Device)> {
    use cpal::traits::{DeviceTrait, HostTrait};

    let device = cpal::default_host().default_output_device()?;
    let config = device.default_output_config().ok()?;
    Some((config.sample_rate(), config.channels(), device))
}

fn build_stream(
    device: &cpal::Device,
    channels: u16,
    sample_rate: u32,
    ring: Arc<SampleRing>,
    clock: Clock,
) -> Result<cpal::Stream> {
    use cpal::traits::DeviceTrait;

    let frames_per_callback = channels.max(1) as usize;
    device
        .build_output_stream::<f32, _, _>(
            cpal::StreamConfig {
                channels,
                sample_rate,
                buffer_size: cpal::BufferSize::Default,
            },
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let read = ring.pop(data);
                if read < data.len() {
                    data[read..].fill(0.0);
                }
                clock.advance_frames((read / frames_per_callback) as u64);
            },
            |error: cpal::Error| log::debug!("audio output error: {error}"),
            None,
        )
        .context("could not build the audio output stream")
}

struct DecodeJob {
    source: Source,
    title: String,
    start_at: f64,
    autoplay: bool,
    window_enabled: bool,
    device_channels: usize,
    device_sample_rate: u32,
    slot: FrameSlot,
    commands: std::sync::mpsc::Receiver<Command>,
    events: std::sync::mpsc::Sender<Event>,
    clock: Clock,
    ring: Arc<SampleRing>,
    volume: Arc<VolumeState>,
}

/// Opens the media: local files directly; origins through a range-feeding
/// custom AVIO context, falling back to a full download when the origin
/// cannot serve ranges.
enum OpenedMedia {
    Native(ffmpeg::format::context::Input),
    Custom(CustomAvio),
}

impl OpenedMedia {
    fn input(&mut self) -> &mut ffmpeg::format::context::Input {
        match self {
            Self::Native(input) => input,
            Self::Custom(custom) => custom
                .input
                .as_mut()
                .expect("the custom AVIO input lives until the media is dropped"),
        }
    }
}

use ffmpeg::format::context::Input as FfmpegInput;

/// True when a decoder has no more frames to give right now.
fn receive_finished(error: ffmpeg::Error) -> bool {
    matches!(
        error,
        ffmpeg::Error::Eof
            | ffmpeg::Error::Other {
                errno: ffmpeg::error::EAGAIN
            }
    )
}

struct VideoSetup {
    index: usize,
    decoder: ffmpeg::codec::decoder::Video,
    width: u32,
    height: u32,
    time_base: ffmpeg::Rational,
}

struct AudioSetup {
    index: usize,
    decoder: ffmpeg::codec::decoder::Audio,
    rate: u32,
    layout: ffmpeg::channel_layout::ChannelLayout,
    format: ffmpeg::format::sample::Sample,
}

fn run_decode(job: DecodeJob) -> Result<()> {
    ffmpeg::init().context("could not initialize the linked FFmpeg libraries")?;
    let mut media = open_source(&job.source)?;
    let duration = {
        let input = media.input();
        (input.duration() > 0).then(|| input.duration() as f64 / 1_000_000.0)
    };

    // Discover the best streams and open their decoders up front; stream
    // borrows do not outlive this block.
    let (mut video_setup, mut audio_setup) = {
        let input = media.input();
        let video_stream = input.streams().best(ffmpeg::media::Type::Video);
        let audio_stream = input.streams().best(ffmpeg::media::Type::Audio);
        let video = video_stream
            .map(|stream| -> Result<VideoSetup> {
                let parameters = stream.parameters();
                let index = stream.index();
                let codec = parameters.id();
                let decoder = ffmpeg::codec::context::Context::from_parameters(parameters)
                    .context("could not read the video stream parameters")?
                    .decoder()
                    .video()
                    .with_context(|| {
                        format!("no decoder is available for {} video", codec.name())
                    })?;
                let time_base = stream.time_base();
                let width = decoder.width();
                let height = decoder.height();
                Ok(VideoSetup {
                    index,
                    decoder,
                    width,
                    height,
                    time_base,
                })
            })
            .transpose()?;
        let audio = audio_stream
            .map(|stream| -> Result<AudioSetup> {
                let parameters = stream.parameters();
                let index = stream.index();
                let codec = parameters.id();
                let decoder = ffmpeg::codec::context::Context::from_parameters(parameters)
                    .context("could not read the audio stream parameters")?
                    .decoder()
                    .audio()
                    .with_context(|| {
                        format!("no decoder is available for {} audio", codec.name())
                    })?;
                let rate = decoder.rate();
                let layout = decoder.channel_layout();
                let format = decoder.format();
                Ok(AudioSetup {
                    index,
                    decoder,
                    rate,
                    layout,
                    format,
                })
            })
            .transpose()?;
        (video, audio)
    };

    let video_index = video_setup.as_ref().map(|setup| setup.index);
    let audio_index = audio_setup.as_ref().map(|setup| setup.index);
    let render_video = video_setup.is_some() && job.window_enabled;
    log::debug!(
        "player: video_index={video_index:?} audio_index={audio_index:?} device_channels={} device_rate={}",
        job.device_channels,
        job.device_sample_rate
    );

    let _ = job.events.send(Event::Opened {
        title: job.title,
        duration,
    });

    let mut resampler = audio_setup
        .as_ref()
        .map(|setup| {
            resampling::Context::get(
                setup.format,
                setup.layout,
                setup.rate,
                ffmpeg::format::sample::Sample::F32(ffmpeg::format::sample::Type::Packed),
                ffmpeg::channel_layout::ChannelLayout::STEREO,
                job.device_sample_rate.max(1),
            )
            .context("could not create the audio resampler")
        })
        .transpose()?;

    let mut scaler = video_setup
        .as_ref()
        .map(|setup| {
            scaling::Context::get(
                setup.decoder.format(),
                setup.width,
                setup.height,
                Pixel::RGBZ,
                setup.width,
                setup.height,
                scaling::Flags::BILINEAR,
            )
            .context("could not create the video scaler")
        })
        .transpose()?;

    // Without an audio track the sample clock has nothing to advance it;
    // fall back to wall-driven timing.
    if audio_setup.is_none() && job.clock.sample_mode() {
        job.clock.set_sample_mode(false);
    }
    job.clock.seek_to(job.start_at);

    let mut playing = job.autoplay;
    job.clock.set_playing(playing);
    // Announce the initial transport state so senders stop waiting in
    // BUFFERING and start showing progress.
    let _ = job.events.send(Event::State(if playing {
        PlaybackState::Playing
    } else {
        PlaybackState::Paused
    }));

    let mut input_eof = false;
    let mut eof_sent = false;
    // Streams without a decoder start out drained; their drain loops never run.
    let mut audio_drained = audio_setup.is_none();
    let mut video_drained = !render_video;
    let mut pending_video: Option<VideoFrame> = None;
    let mut packet = ffmpeg::Packet::empty();
    let mut wait_steps: u32 = 0;

    loop {
        // 1. Commands between packets; seeks reset the pipeline below.
        let mut seek_request: Option<f64> = None;
        while let Ok(command) = job.commands.try_recv() {
            match command {
                Command::Stop => return Ok(()),
                Command::Play => {
                    if !playing {
                        playing = true;
                        job.clock.set_playing(true);
                        let _ = job.events.send(Event::State(PlaybackState::Playing));
                    }
                }
                Command::Pause => {
                    if playing {
                        playing = false;
                        job.clock.set_playing(false);
                        let _ = job.events.send(Event::State(PlaybackState::Paused));
                    }
                }
                Command::SetVolume { level, muted } => job.volume.set(level, muted),
                Command::Seek(target) => seek_request = Some(target),
            }
        }

        if let Some(target) = seek_request {
            seek_media(
                &mut media,
                &mut video_setup,
                &mut audio_setup,
                &mut resampler,
                &mut scaler,
                job.device_sample_rate,
                target,
            )?;
            job.clock.seek_to(target);
            pending_video = None;
            input_eof = false;
            eof_sent = false;
            continue;
        }

        // 2. While paused, hold the pipeline still (the ring stays full, the
        // clock is frozen, and output stays silent).
        if !playing {
            std::thread::sleep(DRAIN_POLL);
            continue;
        }

        // 3. End of media: decoders flushed and drained, audio queue empty.
        if input_eof && audio_drained && (video_drained || !render_video) && pending_video.is_none()
        {
            let finished = if job.clock.sample_mode() {
                job.ring.is_empty()
            } else {
                duration
                    .map(|duration| job.clock.media_time_secs() >= duration - 0.05)
                    .unwrap_or(true)
            };
            if finished {
                let _ = job.events.send(Event::Ended);
                return Ok(());
            }
        }

        // 4. Present the pending video frame when its time has come, or
        // shortly after so slow pacing cannot stall the pipeline.
        if let Some(frame) = &pending_video {
            let due = frame.pts_secs <= job.clock.media_time_secs() + PRESENT_AHEAD_SECS;
            let ring_full = job.ring.len() > RING_HIGH_WATER;
            let waited_too_long = wait_steps >= MAX_PRESENT_WAIT_STEPS;
            if due || ring_full || waited_too_long {
                if let Some(frame) = pending_video.take() {
                    *job.slot.lock().expect("frame slot") = Some(frame);
                }
                wait_steps = 0;
            } else {
                wait_steps += 1;
                std::thread::sleep(DRAIN_POLL);
                continue;
            }
        }

        // 5. Audio backpressure: wait while the ring holds plenty of audio.
        if job.ring.len() > RING_HIGH_WATER && !input_eof {
            std::thread::sleep(DRAIN_POLL);
            continue;
        }

        if input_eof {
            std::thread::sleep(DRAIN_POLL);
            continue;
        }

        if log::log_enabled!(log::Level::Trace) {
            log::trace!(
                "player: ring={} clock={:.2}",
                job.ring.len(),
                job.clock.media_time_secs()
            );
        }

        // 6. Read one packet. The iterator abstraction cannot coexist with
        // seeking, so the read goes through the raw context handle.
        let read_result =
            unsafe { ffmpeg::ffi::av_read_frame(media.input().as_mut_ptr(), packet.as_mut_ptr()) };
        if read_result == ffmpeg::ffi::AVERROR_EOF {
            input_eof = true;
            continue;
        } else if read_result == ffmpeg::ffi::AVERROR(libc::EAGAIN) {
            continue;
        } else if read_result < 0 {
            return Err(anyhow!(
                "reading the media stream failed (code {read_result})"
            ));
        }
        let packet_stream = packet.stream();

        if Some(packet_stream) == audio_index {
            if let Some(setup) = &mut audio_setup {
                setup
                    .decoder
                    .send_packet(&packet)
                    .context("the audio decoder rejected a packet")?;
            }
        } else if Some(packet_stream) == video_index
            && render_video
            && let Some(setup) = &mut video_setup
        {
            setup
                .decoder
                .send_packet(&packet)
                .context("the video decoder rejected a packet")?;
        }

        // 7. Flush EOF into the decoders once the input is exhausted.
        if input_eof && !eof_sent {
            eof_sent = true;
            if let Some(setup) = &mut audio_setup {
                let _ = setup.decoder.send_eof();
            }
            if let Some(setup) = &mut video_setup {
                let _ = setup.decoder.send_eof();
            }
            continue;
        }

        // 8. Drain the audio decoder into the ring.
        if let Some(setup) = &mut audio_setup {
            let mut decoded = ffmpeg::frame::Audio::empty();
            loop {
                match setup.decoder.receive_frame(&mut decoded) {
                    Ok(()) => {
                        let converted = resample(&mut resampler, &decoded, job.device_sample_rate)?;
                        if let Some(converted) = converted {
                            job.ring.push(&converted);
                        }
                    }
                    Err(error) if receive_finished(error) => {
                        audio_drained = true;
                        break;
                    }
                    Err(error) => return Err(error).context("audio decoding failed"),
                }
            }
        }

        // 9. Flush the video decoder and hold the newest frame for its slot.
        if render_video && let Some(setup) = &mut video_setup {
            let mut decoded = ffmpeg::frame::Video::empty();
            loop {
                match setup.decoder.receive_frame(&mut decoded) {
                    Ok(()) => {
                        let secs = decoded
                            .timestamp()
                            .map(|ts| {
                                ts as f64 * setup.time_base.numerator() as f64
                                    / setup.time_base.denominator().max(1) as f64
                            })
                            .unwrap_or_else(|| job.clock.media_time_secs());
                        if let Some(frame) = scale(&mut scaler, setup, &decoded, secs)? {
                            pending_video = Some(frame);
                        }
                    }
                    Err(error) if receive_finished(error) => {
                        video_drained = true;
                        break;
                    }
                    Err(error) => return Err(error).context("video decoding failed"),
                }
            }
        }
    }
}

/// Seeks the input and resets every decoder-side buffer so playback restarts
/// cleanly at `target` seconds.
#[allow(clippy::too_many_arguments)]
fn seek_media(
    media: &mut OpenedMedia,
    video_setup: &mut Option<VideoSetup>,
    audio_setup: &mut Option<AudioSetup>,
    resampler: &mut Option<resampling::Context>,
    scaler: &mut Option<scaling::Context>,
    output_rate: u32,
    target: f64,
) -> Result<()> {
    let timestamp = (target.max(0.0) * 1_000_000.0) as i64;
    media
        .input()
        .seek(timestamp, ..)
        .context("could not seek the media stream")?;
    if let Some(setup) = video_setup {
        setup.decoder.flush();
    }
    if let Some(setup) = audio_setup {
        setup.decoder.flush();
    }
    // The resampler and scaler may hold buffered source-format data from the
    // old position; both are recreated rather than partially drained.
    if let Some(setup) = audio_setup {
        *resampler = Some(
            resampling::Context::get(
                setup.format,
                setup.layout,
                setup.rate,
                ffmpeg::format::sample::Sample::F32(ffmpeg::format::sample::Type::Packed),
                ffmpeg::channel_layout::ChannelLayout::STEREO,
                output_rate.max(1),
            )
            .context("could not recreate the audio resampler")?,
        );
    }
    if let Some(setup) = video_setup {
        *scaler = Some(
            scaling::Context::get(
                setup.decoder.format(),
                setup.width,
                setup.height,
                Pixel::RGBZ,
                setup.width,
                setup.height,
                scaling::Flags::BILINEAR,
            )
            .context("could not recreate the video scaler")?,
        );
    }
    Ok(())
}

/// Converts a decoded audio frame to interleaved stereo f32 samples.
fn resample(
    resampler: &mut Option<resampling::Context>,
    decoded: &ffmpeg::frame::Audio,
    output_rate: u32,
) -> Result<Option<Vec<f32>>> {
    let Some(resampler) = resampler else {
        return Ok(None);
    };
    let capacity = (decoded.samples() as u64 * u64::from(output_rate)
        / u64::from(decoded.rate().max(1))
        + 256) as usize;
    let mut converted = ffmpeg::frame::Audio::new(
        ffmpeg::format::sample::Sample::F32(ffmpeg::format::sample::Type::Packed),
        capacity.max(1),
        ffmpeg::channel_layout::ChannelLayout::STEREO,
    );
    resampler
        .run(decoded, &mut converted)
        .context("could not resample a decoded audio frame")?;
    let samples = converted.samples();
    if samples == 0 {
        return Ok(None);
    }
    let bytes = converted.data(0);
    let wanted = samples * 2 * std::mem::size_of::<f32>();
    let mut out = vec![0.0_f32; samples * 2];
    let byte_slice = &bytes[..wanted.min(bytes.len())];
    for (chunk, target) in byte_slice.chunks_exact(4).zip(out.iter_mut()) {
        *target = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    Ok(Some(out))
}

/// Scales a decoded video frame to packed RGB pixels for softbuffer.
fn scale(
    scaler: &mut Option<scaling::Context>,
    setup: &VideoSetup,
    decoded: &ffmpeg::frame::Video,
    pts_secs: f64,
) -> Result<Option<VideoFrame>> {
    // The codec's nominal format can differ from what the decoder actually
    // outputs, so the scaler is rebuilt from the frame itself when needed.
    let rebuild = scaler
        .as_ref()
        .map(|context| {
            context.input().format != decoded.format()
                || context.input().width != decoded.width()
                || context.input().height != decoded.height()
        })
        .unwrap_or(true);
    if rebuild {
        *scaler = Some(
            scaling::Context::get(
                decoded.format(),
                decoded.width(),
                decoded.height(),
                Pixel::RGBZ,
                setup.width.max(1),
                setup.height.max(1),
                scaling::Flags::BILINEAR,
            )
            .context("could not rebuild the video scaler")?,
        );
    }
    let Some(scaler) = scaler else {
        return Ok(None);
    };
    let width = decoded.width();
    let height = decoded.height();
    let mut converted = ffmpeg::frame::Video::new(Pixel::RGBZ, width, height);
    scaler
        .run(decoded, &mut converted)
        .context("could not convert a decoded video frame")?;
    let stride = converted.stride(0);
    let bytes = converted.data(0);
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for row in 0..height {
        let row_start = (row * stride as u32) as usize;
        for column in 0..width {
            let offset = row_start + (column * 4) as usize;
            if offset + 3 < bytes.len() {
                pixels.push(u32::from_le_bytes([
                    bytes[offset],
                    bytes[offset + 1],
                    bytes[offset + 2],
                    bytes[offset + 3],
                ]));
            }
        }
    }
    Ok(Some(VideoFrame {
        width,
        height,
        pixels,
        pts_secs,
    }))
}
/// A custom AVIO context feeding ffmpeg from an HTTP origin.
struct CustomAvio {
    input: Option<FfmpegInput>,
    pb: *mut ffmpeg::ffi::AVIOContext,
    opaque: *mut AvioReader,
}

struct AvioReader {
    reader: RangeReader,
    position: u64,
}

unsafe impl Send for CustomAvio {}

impl Drop for CustomAvio {
    fn drop(&mut self) {
        unsafe {
            // Dropping the input closes the format context but leaves the
            // custom pb alone (AVFMT_FLAG_CUSTOM_IO); free it ourselves.
            if let Some(input) = self.input.take() {
                drop(input);
            }
            // Contexts from avio_alloc_context must be freed with
            // avio_context_free, not avio_close (which expects avio_open
            // contexts and crashes on these).
            if !self.pb.is_null() {
                ffmpeg::ffi::avio_context_free(&mut self.pb);
            }
            if !self.opaque.is_null() {
                drop(Box::from_raw(self.opaque));
            }
        }
    }
}

extern "C" fn avio_read_cb(opaque: *mut std::ffi::c_void, buf: *mut u8, buf_size: i32) -> i32 {
    unsafe {
        let reader = &mut *(opaque as *mut AvioReader);
        let slice = std::slice::from_raw_parts_mut(buf, buf_size.max(0) as usize);
        match reader.reader.read_at(reader.position, slice) {
            Ok(read) => {
                reader.position += read as u64;
                read as i32
            }
            // Zero signals end-of-data to ffmpeg; surfacing I/O errors here
            // sends demuxer teardown down re-entrant paths that misbehave.
            Err(error) => {
                log::debug!("media read at {} failed: {error:#}", reader.position);
                0
            }
        }
    }
}

extern "C" fn avio_seek_cb(opaque: *mut std::ffi::c_void, offset: i64, whence: i32) -> i64 {
    unsafe {
        let reader = &mut *(opaque as *mut AvioReader);
        const AVSEEK_SIZE: i32 = 0x10000;
        if whence & AVSEEK_SIZE != 0 {
            return reader.reader.size().map(|size| size as i64).unwrap_or(-1);
        }
        let target = match whence {
            0 => offset,                          // SEEK_SET
            1 => reader.position as i64 + offset, // SEEK_CUR
            2 => match reader.reader.size() {
                // SEEK_END
                Some(size) => size as i64 + offset,
                None => return -1,
            },
            _ => return -1,
        };
        if target < 0 {
            return -1;
        }
        reader.position = target as u64;
        target
    }
}

fn open_source(source: &Source) -> Result<OpenedMedia> {
    match source {
        Source::LocalFile(path) => Ok(OpenedMedia::Native(
            ffmpeg::format::input(&path)
                .with_context(|| format!("could not open {}", path.display()))?,
        )),
        Source::Origin(url) => {
            let reader = RangeReader::open(url)?;
            if !reader.supports_ranges() {
                let path = reader.download_to_temp()?;
                return Ok(OpenedMedia::Native(
                    ffmpeg::format::input(&path).context("could not open the downloaded media")?,
                ));
            }
            let mut avio = open_custom_avio(url, reader)?;
            // Populate the codec parameters the demuxer's header alone may
            // not have probed (frame rates, pixel formats, dimensions).
            unsafe {
                let mut input = avio
                    .input
                    .take()
                    .expect("the custom input lives until dropped");
                ffmpeg::ffi::avformat_find_stream_info(input.as_mut_ptr(), std::ptr::null_mut());
                avio.input = Some(input);
            }
            Ok(OpenedMedia::Custom(avio))
        }
    }
}

fn open_custom_avio(url: &str, reader: RangeReader) -> Result<CustomAvio> {
    unsafe {
        let opaque = Box::into_raw(Box::new(AvioReader {
            reader,
            position: 0,
        }));
        let buffer = ffmpeg::ffi::av_malloc(AVIO_BUFFER_SIZE) as *mut u8;
        if buffer.is_null() {
            drop(Box::from_raw(opaque));
            return Err(anyhow!("could not allocate the media read buffer"));
        }
        let pb = ffmpeg::ffi::avio_alloc_context(
            buffer,
            AVIO_BUFFER_SIZE as i32,
            0,
            opaque as *mut std::ffi::c_void,
            Some(avio_read_cb),
            None,
            Some(avio_seek_cb),
        );
        if pb.is_null() {
            ffmpeg::ffi::av_free(buffer as *mut std::ffi::c_void);
            drop(Box::from_raw(opaque));
            return Err(anyhow!("could not create the media read context"));
        }
        let mut context = ffmpeg::ffi::avformat_alloc_context();
        if context.is_null() {
            ffmpeg::ffi::avio_context_free(&mut pb.cast());
            drop(Box::from_raw(opaque));
            return Err(anyhow!("could not allocate the media format context"));
        }
        (*context).pb = pb;
        (*context).flags |= ffmpeg::ffi::AVFMT_FLAG_CUSTOM_IO;
        let rc = ffmpeg::ffi::avformat_open_input(
            &mut context,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if rc != 0 {
            ffmpeg::ffi::avio_context_free(&mut pb.cast());
            drop(Box::from_raw(opaque));
            return Err(anyhow!("could not open the media stream from {url}"));
        }
        let input = FfmpegInput::wrap(context);
        Ok(CustomAvio {
            input: Some(input),
            pb,
            opaque,
        })
    }
}
