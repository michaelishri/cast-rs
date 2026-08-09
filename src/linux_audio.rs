use std::{
    collections::VecDeque,
    io::Cursor,
    mem,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use ffmpeg::{ChannelLayout, Packet, codec, encoder, frame};
use ffmpeg_next as ffmpeg;
use pipewire as pw;
use pw::{properties::properties, spa};

use crate::desktop::{
    EncodedAudioFrame, LocalOutputBackend, LocalOutputControl,
    LocalOutputRedirect as SharedLocalOutputRedirect, OutputSnapshot,
};
use crate::linux_pipewire::CaptureEpoch;

pub(crate) const SAMPLE_RATE: u32 = 48_000;
pub(crate) const CHANNELS: u32 = 2;
pub(crate) const BITRATE: u32 = 192_000;
pub(crate) const ACCESS_UNIT_SAMPLES: usize = 1_024;
pub(crate) const RTP_PAYLOAD_TYPE: u8 = 97;

const AUDIO_QUEUE_CAPACITY: usize = 64;
const MAX_CONTINUOUS_SILENCE_SAMPLES: u64 = SAMPLE_RATE as u64 * 2;
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) struct PreparedAudioEncoder(AacEncoder);

pub(crate) fn prepare() -> Result<PreparedAudioEncoder> {
    AacEncoder::new().map(PreparedAudioEncoder)
}

enum AudioCommand {
    Pcm {
        timestamp: u64,
        interleaved: Vec<f32>,
    },
    AdvanceTo(u64),
}

#[derive(Clone)]
pub(crate) struct AudioSubmitter {
    sender: SyncSender<AudioCommand>,
    failure: Arc<Mutex<Option<String>>>,
}

impl AudioSubmitter {
    fn submit_pcm(&self, timestamp: u64, interleaved: Vec<f32>) {
        self.try_send(
            AudioCommand::Pcm {
                timestamp,
                interleaved,
            },
            "audio capture queue is full",
        );
    }

    pub(crate) fn advance_to(&self, timestamp: u64) {
        self.try_send(
            AudioCommand::AdvanceTo(timestamp),
            "audio capture queue is full while aligning an HLS segment",
        );
    }

    fn try_send(&self, command: AudioCommand, full_message: &str) {
        match self.sender.try_send(command) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => store_failure(&self.failure, full_message),
            Err(TrySendError::Disconnected(_)) => {
                store_failure(&self.failure, "audio encoder worker stopped unexpectedly");
            }
        }
    }
}

pub(crate) struct AudioWorker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioWorker {
    pub(crate) fn start_prepared<F>(
        encoder: PreparedAudioEncoder,
        failure: Arc<Mutex<Option<String>>>,
        mut output: F,
    ) -> Result<(AudioSubmitter, Self)>
    where
        F: FnMut(EncodedAudioFrame) -> Result<()> + Send + 'static,
    {
        let PreparedAudioEncoder(encoder) = encoder;
        let (sender, receiver) = mpsc::sync_channel(AUDIO_QUEUE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_failure = Arc::clone(&failure);
        let thread = thread::Builder::new()
            .name("desktop-aac-encoder".into())
            .spawn(move || {
                let mut pipeline = AudioPipeline::new(encoder);
                while !worker_stop.load(Ordering::SeqCst) {
                    let command = match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    let result = match command {
                        AudioCommand::Pcm {
                            timestamp,
                            interleaved,
                        } => pipeline.push_pcm(timestamp, &interleaved, &mut output),
                        AudioCommand::AdvanceTo(timestamp) => {
                            pipeline.advance_to(timestamp, &mut output)
                        }
                    };
                    if let Err(error) = result {
                        store_failure(&worker_failure, &format!("desktop audio failed: {error:#}"));
                        break;
                    }
                }
            })
            .context("could not start AAC encoder worker")?;
        Ok((
            AudioSubmitter { sender, failure },
            Self {
                stop,
                thread: Some(thread),
            },
        ))
    }

    pub(crate) fn stop(mut self) -> Result<()> {
        self.finish()
    }

    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("AAC encoder worker panicked"))?;
        }
        Ok(())
    }
}

impl Drop for AudioWorker {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            log::warn!("could not stop Linux AAC encoder: {error:#}");
        }
    }
}

struct AudioPipeline {
    encoder: AacEncoder,
    left: VecDeque<f32>,
    right: VecDeque<f32>,
    fifo_start: u64,
    next_sample: u64,
}

impl AudioPipeline {
    fn new(encoder: AacEncoder) -> Self {
        Self {
            encoder,
            left: VecDeque::new(),
            right: VecDeque::new(),
            fifo_start: 0,
            next_sample: 0,
        }
    }

    fn push_pcm<F>(&mut self, captured_at: u64, pcm: &[f32], output: &mut F) -> Result<()>
    where
        F: FnMut(EncodedAudioFrame) -> Result<()>,
    {
        if !pcm.len().is_multiple_of(CHANNELS as usize) {
            bail!("PipeWire returned a truncated stereo audio buffer");
        }
        let timestamp = captured_at;
        if timestamp > self.next_sample {
            self.push_silence(timestamp - self.next_sample, output)?;
        }
        let frames = pcm.len() / CHANNELS as usize;
        let trim = usize::try_from(self.next_sample.saturating_sub(timestamp))
            .unwrap_or(frames)
            .min(frames);
        for pair in pcm.chunks_exact(CHANNELS as usize).skip(trim) {
            self.left.push_back(pair[0]);
            self.right.push_back(pair[1]);
        }
        self.next_sample = self
            .next_sample
            .saturating_add(u64::try_from(frames.saturating_sub(trim)).unwrap_or(u64::MAX));
        self.drain(output)
    }

    fn advance_to<F>(&mut self, timestamp: u64, output: &mut F) -> Result<()>
    where
        F: FnMut(EncodedAudioFrame) -> Result<()>,
    {
        let aligned = timestamp.saturating_add(ACCESS_UNIT_SAMPLES as u64 - 1)
            / ACCESS_UNIT_SAMPLES as u64
            * ACCESS_UNIT_SAMPLES as u64;
        if aligned > self.next_sample {
            self.push_silence(aligned - self.next_sample, output)?;
        }
        self.drain(output)
    }

    fn push_silence<F>(&mut self, mut frames: u64, output: &mut F) -> Result<()>
    where
        F: FnMut(EncodedAudioFrame) -> Result<()>,
    {
        if frames > MAX_CONTINUOUS_SILENCE_SAMPLES {
            let target = self.next_sample.saturating_add(frames);
            let start = target.saturating_sub(ACCESS_UNIT_SAMPLES as u64);
            self.left.clear();
            self.right.clear();
            self.fifo_start = start;
            self.next_sample = start;
            frames = ACCESS_UNIT_SAMPLES as u64;
        }
        let frames = usize::try_from(frames).unwrap_or(usize::MAX);
        self.left.extend(std::iter::repeat_n(0.0, frames));
        self.right.extend(std::iter::repeat_n(0.0, frames));
        self.next_sample = self.next_sample.saturating_add(frames as u64);
        self.drain(output)
    }

    fn drain<F>(&mut self, output: &mut F) -> Result<()>
    where
        F: FnMut(EncodedAudioFrame) -> Result<()>,
    {
        while self.left.len() >= ACCESS_UNIT_SAMPLES && self.right.len() >= ACCESS_UNIT_SAMPLES {
            let left: [f32; ACCESS_UNIT_SAMPLES] =
                std::array::from_fn(|_| self.left.pop_front().unwrap());
            let right: [f32; ACCESS_UNIT_SAMPLES] =
                std::array::from_fn(|_| self.right.pop_front().unwrap());
            let timestamp = self.fifo_start;
            self.fifo_start = self.fifo_start.saturating_add(ACCESS_UNIT_SAMPLES as u64);
            if let Some((timestamp, data)) = self.encoder.encode(&left, &right, timestamp)? {
                output(EncodedAudioFrame { timestamp, data })?;
            }
        }
        Ok(())
    }
}

struct AacEncoder {
    encoder: encoder::audio::Encoder,
}

impl AacEncoder {
    fn new() -> Result<Self> {
        ffmpeg::init().context("could not initialize FFmpeg for AAC")?;
        let codec = encoder::find(codec::Id::AAC)
            .ok_or_else(|| anyhow!("linked FFmpeg libraries do not provide an AAC encoder"))?;
        let format = ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Planar);
        if !codec
            .audio()
            .context("selected AAC encoder is not an audio encoder")?
            .formats()
            .is_some_and(|formats| formats.into_iter().any(|candidate| candidate == format))
        {
            bail!("linked AAC encoder does not accept planar 32-bit float audio");
        }
        let mut encoder = codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .context("could not configure the AAC encoder")?;
        encoder.set_rate(SAMPLE_RATE as i32);
        encoder.set_channel_layout(ChannelLayout::STEREO);
        encoder.set_format(format);
        encoder.set_bit_rate(BITRATE as usize);
        encoder.set_max_bit_rate(BITRATE as usize);
        encoder.set_time_base((1, SAMPLE_RATE as i32));
        let encoder = encoder
            .open_as(codec)
            .context("could not open the FFmpeg AAC-LC encoder")?;
        if encoder.frame_size() as usize != ACCESS_UNIT_SAMPLES {
            bail!(
                "AAC encoder uses {} samples per access unit instead of {ACCESS_UNIT_SAMPLES}",
                encoder.frame_size()
            );
        }
        Ok(Self { encoder })
    }

    fn encode(
        &mut self,
        left: &[f32; ACCESS_UNIT_SAMPLES],
        right: &[f32; ACCESS_UNIT_SAMPLES],
        timestamp: u64,
    ) -> Result<Option<(u64, Vec<u8>)>> {
        let mut input = frame::Audio::new(
            self.encoder.format(),
            ACCESS_UNIT_SAMPLES,
            ChannelLayout::STEREO,
        );
        input.set_rate(SAMPLE_RATE);
        input.set_pts(Some(i64::try_from(timestamp).unwrap_or(i64::MAX)));
        input.plane_mut::<f32>(0).copy_from_slice(left);
        input.plane_mut::<f32>(1).copy_from_slice(right);
        self.encoder
            .send_frame(&input)
            .context("AAC encoder rejected desktop PCM")?;
        let mut packet = Packet::empty();
        match self.encoder.receive_packet(&mut packet) {
            Ok(()) => {
                let payload = packet
                    .data()
                    .ok_or_else(|| anyhow!("AAC encoder returned an empty packet"))?;
                let mut framed = adts_header(payload.len())?;
                framed.extend_from_slice(payload);
                let timestamp = packet
                    .pts()
                    .and_then(|pts| u64::try_from(pts).ok())
                    .unwrap_or(timestamp);
                Ok(Some((timestamp, framed)))
            }
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => Ok(None),
            Err(error) => Err(error).context("AAC encoding failed"),
        }
    }
}

fn adts_header(payload_length: usize) -> Result<Vec<u8>> {
    let frame_length = payload_length
        .checked_add(7)
        .ok_or_else(|| anyhow!("AAC packet length overflow"))?;
    if frame_length > 0x1fff {
        bail!("AAC packet is too large for ADTS");
    }
    let profile = 1_u8;
    let frequency_index = 3_u8;
    let channel_configuration = CHANNELS as u8;
    Ok(vec![
        0xff,
        0xf1,
        (profile << 6) | (frequency_index << 2) | (channel_configuration >> 2),
        ((channel_configuration & 3) << 6) | ((frame_length >> 11) as u8 & 3),
        (frame_length >> 3) as u8,
        ((frame_length as u8 & 7) << 5) | 0x1f,
        0xfc,
    ])
}

struct PipeWireAudioData {
    format: spa::param::audio::AudioInfoRaw,
    submitter: AudioSubmitter,
    sample_cursor: Option<u64>,
    capture_epoch: CaptureEpoch,
    failure: Arc<Mutex<Option<String>>>,
    startup: Option<mpsc::Sender<Result<(), String>>>,
    started: Arc<AtomicBool>,
}

pub(crate) struct PipeWireAudioCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PipeWireAudioCapture {
    pub(crate) fn start(
        submitter: AudioSubmitter,
        failure: Arc<Mutex<Option<String>>>,
        capture_epoch: CaptureEpoch,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_failure = Arc::clone(&failure);
        let started = Arc::new(AtomicBool::new(false));
        let thread_started = Arc::clone(&started);
        let (startup_tx, startup_rx) = mpsc::channel();
        let startup_fallback = startup_tx.clone();
        let thread = thread::Builder::new()
            .name("cast-pipewire-audio".to_owned())
            .spawn(move || {
                if let Err(error) = run_pipewire_audio(
                    submitter,
                    capture_epoch,
                    &thread_stop,
                    Arc::clone(&thread_failure),
                    startup_tx,
                    Arc::clone(&thread_started),
                ) {
                    let message = format!("{error:#}");
                    if thread_started.load(Ordering::SeqCst) {
                        store_failure(&thread_failure, &message);
                    } else {
                        let _ = startup_fallback.send(Err(message));
                    }
                }
            })
            .context("could not start the PipeWire audio thread")?;
        match startup_rx.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(())) => {
                started.store(true, Ordering::SeqCst);
                Ok(Self {
                    stop,
                    thread: Some(thread),
                })
            }
            Ok(Err(error)) => {
                stop.store(true, Ordering::SeqCst);
                let _ = thread.join();
                bail!("could not start PipeWire sink-monitor capture: {error}");
            }
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                let _ = thread.join();
                bail!("timed out starting PipeWire sink-monitor capture");
            }
        }
    }

    pub(crate) fn stop(mut self) -> Result<()> {
        self.finish()
    }

    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("PipeWire audio capture thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for PipeWireAudioCapture {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            log::warn!("could not stop PipeWire audio capture: {error:#}");
        }
    }
}

fn run_pipewire_audio(
    submitter: AudioSubmitter,
    capture_epoch: CaptureEpoch,
    stop: &Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    startup: mpsc::Sender<Result<(), String>>,
    started: Arc<AtomicBool>,
) -> Result<()> {
    pw::init();
    let mainloop =
        pw::main_loop::MainLoopRc::new(None).context("could not create the PipeWire audio loop")?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .context("could not create the PipeWire audio context")?;
    let core = context
        .connect_rc(None)
        .context("could not connect to the PipeWire desktop audio service")?;
    let stream = pw::stream::StreamRc::new(
        core,
        "cast desktop audio",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::STREAM_CAPTURE_SINK => "true",
        },
    )
    .context("could not create the PipeWire sink-monitor stream")?;
    let loop_for_state = mainloop.downgrade();
    let listener = stream
        .add_local_listener_with_user_data(PipeWireAudioData {
            format: Default::default(),
            submitter,
            sample_cursor: None,
            capture_epoch,
            failure: Arc::clone(&failure),
            startup: Some(startup),
            started,
        })
        .state_changed(move |_, data, _, state| {
            if let pw::stream::StreamState::Error(message) = state {
                report_stream_error(&mut data.startup, &data.started, &data.failure, &message);
                if let Some(mainloop) = loop_for_state.upgrade() {
                    mainloop.quit();
                }
            }
        })
        .param_changed(|_, data, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if let Err(error) = data.format.parse(param) {
                store_failure(
                    &data.failure,
                    &format!("could not parse PipeWire audio format: {error:?}"),
                );
                return;
            }
            if data.format.format() != spa::param::audio::AudioFormat::F32LE
                || data.format.rate() != SAMPLE_RATE
                || data.format.channels() != CHANNELS
            {
                store_failure(
                    &data.failure,
                    &format!(
                        "unsupported PipeWire audio format: {:?}, {} Hz, {} channels",
                        data.format.format(),
                        data.format.rate(),
                        data.format.channels()
                    ),
                );
            }
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            if let Some(startup) = data.startup.take() {
                data.started.store(true, Ordering::SeqCst);
                let _ = startup.send(Ok(()));
            }
            if let Err(error) = copy_pipewire_audio(&mut buffer, data) {
                store_failure(
                    &data.failure,
                    &format!("could not copy PipeWire audio: {error:#}"),
                );
            }
        })
        .register()
        .context("could not register PipeWire audio callbacks")?;
    let values = audio_format_parameter()?;
    let mut params = [spa::pod::Pod::from_bytes(&values)
        .ok_or_else(|| anyhow!("could not construct PipeWire audio parameters"))?];
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("could not connect to the default PipeWire sink monitor")?;
    let loop_for_timer = mainloop.downgrade();
    let timer_stop = Arc::clone(stop);
    let timer = mainloop.loop_().add_timer(move |_| {
        if timer_stop.load(Ordering::SeqCst)
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
        .context("could not arm the PipeWire audio shutdown timer")?;
    mainloop.run();
    drop(timer);
    drop(listener);
    stream
        .disconnect()
        .context("could not disconnect the PipeWire audio stream")
}

fn report_stream_error(
    startup: &mut Option<mpsc::Sender<Result<(), String>>>,
    started: &AtomicBool,
    failure: &Mutex<Option<String>>,
    message: &str,
) {
    if let Some(startup) = startup.take() {
        let _ = startup.send(Err(message.to_owned()));
    } else if started.load(Ordering::SeqCst) {
        store_failure(failure, &format!("PipeWire audio stream error: {message}"));
    }
}

fn audio_format_parameter() -> Result<Vec<u8>> {
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(SAMPLE_RATE);
    audio_info.set_channels(CHANNELS);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|error| anyhow!("could not serialize PipeWire audio parameters: {error:?}"))
}

fn copy_pipewire_audio(
    buffer: &mut pw::buffer::Buffer<'_>,
    data: &mut PipeWireAudioData,
) -> Result<()> {
    if data.format.format() != spa::param::audio::AudioFormat::F32LE
        || data.format.rate() != SAMPLE_RATE
        || data.format.channels() != CHANNELS
    {
        return Ok(());
    }
    let plane = buffer
        .datas_mut()
        .first_mut()
        .ok_or_else(|| anyhow!("PipeWire audio buffer has no data plane"))?;
    let chunk = plane.chunk();
    let offset = usize::try_from(chunk.offset()).context("audio offset exceeded usize")?;
    let size = usize::try_from(chunk.size()).context("audio size exceeded usize")?;
    let bytes = plane
        .data()
        .ok_or_else(|| anyhow!("PipeWire audio buffer is not memory mapped"))?;
    let end = offset
        .checked_add(size)
        .ok_or_else(|| anyhow!("PipeWire audio buffer range overflow"))?;
    let bytes = bytes
        .get(offset..end)
        .ok_or_else(|| anyhow!("PipeWire audio buffer is truncated"))?;
    if !bytes
        .len()
        .is_multiple_of(mem::size_of::<f32>() * CHANNELS as usize)
    {
        bail!("PipeWire audio buffer does not contain whole stereo frames");
    }
    let pcm = bytes
        .chunks_exact(mem::size_of::<f32>())
        .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()))
        .collect::<Vec<_>>();
    let frames = u64::try_from(pcm.len() / CHANNELS as usize).unwrap_or(u64::MAX);
    let timestamp = data.sample_cursor.unwrap_or_else(|| {
        first_audio_timestamp(data.capture_epoch.ticks(u64::from(SAMPLE_RATE)), frames)
    });
    data.submitter.submit_pcm(timestamp, pcm);
    data.sample_cursor = Some(timestamp.saturating_add(frames));
    Ok(())
}

fn first_audio_timestamp(buffer_end: u64, frames: u64) -> u64 {
    buffer_end.saturating_sub(frames)
}

struct WirePlumberOutputBackend;

impl LocalOutputBackend for WirePlumberOutputBackend {
    fn snapshot(&mut self) -> Result<OutputSnapshot> {
        let inspect = wpctl(&["inspect", "@DEFAULT_AUDIO_SINK@"])?;
        let device_id = parse_wpctl_device_id(&inspect)?;
        let volume = wpctl(&["get-volume", &device_id.to_string()])?;
        let (volume, muted) = parse_wpctl_volume(&volume)?;
        Ok(OutputSnapshot {
            device_id,
            volume: Some(volume),
            muted: Some(muted),
        })
    }

    fn set_volume(&mut self, device_id: u32, volume: f32) -> Result<()> {
        wpctl(&[
            "set-volume",
            &device_id.to_string(),
            &format!("{:.4}", volume.clamp(0.0, 1.0)),
        ])?;
        Ok(())
    }

    fn set_muted(&mut self, device_id: u32, muted: bool) -> Result<()> {
        wpctl(&[
            "set-mute",
            &device_id.to_string(),
            if muted { "1" } else { "0" },
        ])?;
        Ok(())
    }
}

fn wpctl(arguments: &[&str]) -> Result<String> {
    let output = Command::new("wpctl")
        .args(arguments)
        .output()
        .context("WirePlumber's wpctl command is required for local output control")?;
    if !output.status.success() {
        bail!(
            "wpctl {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("wpctl returned non-UTF-8 output")
}

fn parse_wpctl_device_id(output: &str) -> Result<u32> {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("id "))
        .and_then(|rest| rest.split(',').next())
        .and_then(|id| id.trim().parse().ok())
        .ok_or_else(|| anyhow!("wpctl could not resolve the default PipeWire sink"))
}

fn parse_wpctl_volume(output: &str) -> Result<(f32, bool)> {
    let mut words = output.split_whitespace();
    if words.next() != Some("Volume:") {
        bail!("wpctl returned an unexpected volume response");
    }
    let volume = words
        .next()
        .ok_or_else(|| anyhow!("wpctl omitted the output volume"))?
        .parse::<f32>()
        .context("wpctl returned an invalid output volume")?;
    Ok((volume.clamp(0.0, 1.0), output.contains("[MUTED]")))
}

pub(crate) struct LocalOutputRedirect(SharedLocalOutputRedirect<WirePlumberOutputBackend>);

impl LocalOutputRedirect {
    pub(crate) fn start<F>(control: F) -> Result<Self>
    where
        F: FnMut(LocalOutputControl) + Send + 'static,
    {
        SharedLocalOutputRedirect::start(WirePlumberOutputBackend, OUTPUT_POLL_INTERVAL, control)
            .map(Self)
    }

    pub(crate) fn stop(self) -> Result<()> {
        self.0.stop()
    }
}

fn store_failure(failure: &Mutex<Option<String>>, message: &str) {
    let mut failure = failure.lock().unwrap_or_else(|error| error.into_inner());
    if failure.is_none() {
        *failure = Some(message.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wireplumber_sink_state() {
        assert_eq!(
            parse_wpctl_device_id("id 42, type PipeWire:Interface:Node\n").unwrap(),
            42
        );
        assert_eq!(
            parse_wpctl_volume("Volume: 0.625 [MUTED]\n").unwrap(),
            (0.625, true)
        );
        assert_eq!(
            parse_wpctl_volume("Volume: 0.250\n").unwrap(),
            (0.25, false)
        );
    }

    #[test]
    fn adts_header_describes_aac_lc_stereo_at_48_khz() {
        let header = adts_header(100).unwrap();
        assert_eq!(&header[..2], &[0xff, 0xf1]);
        assert_eq!((header[2] >> 2) & 0xf, 3);
        assert_eq!(((header[2] & 1) << 2) | (header[3] >> 6), 2);
    }

    #[test]
    fn pcm_pipeline_repairs_gaps_and_rejects_partial_stereo_frames() {
        let encoder = match AacEncoder::new() {
            Ok(encoder) => encoder,
            Err(error) => {
                eprintln!("skipping AAC runtime test: {error:#}");
                return;
            }
        };
        let mut pipeline = AudioPipeline::new(encoder);
        let mut frames = Vec::new();
        pipeline
            .push_pcm(0, &vec![0.0; ACCESS_UNIT_SAMPLES * 2], &mut |frame| {
                frames.push(frame);
                Ok(())
            })
            .unwrap();
        assert_eq!(pipeline.next_sample, ACCESS_UNIT_SAMPLES as u64);
        pipeline
            .push_pcm(
                (ACCESS_UNIT_SAMPLES * 2) as u64,
                &vec![0.0; ACCESS_UNIT_SAMPLES * 2],
                &mut |frame| {
                    frames.push(frame);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(pipeline.next_sample, (ACCESS_UNIT_SAMPLES * 3) as u64);
        pipeline
            .push_pcm(
                (ACCESS_UNIT_SAMPLES * 2 + ACCESS_UNIT_SAMPLES / 2) as u64,
                &vec![0.0; ACCESS_UNIT_SAMPLES * 2],
                &mut |frame| {
                    frames.push(frame);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(
            pipeline.next_sample,
            (ACCESS_UNIT_SAMPLES * 3 + ACCESS_UNIT_SAMPLES / 2) as u64
        );
        assert!(!frames.is_empty());
        assert!(pipeline.push_pcm(0, &[0.0], &mut |_| Ok(())).is_err());
    }

    #[test]
    fn first_audio_buffer_preserves_its_offset_from_the_shared_epoch() {
        assert_eq!(first_audio_timestamp(24_000, 480), 23_520);
        assert_eq!(first_audio_timestamp(240, 480), 0);
    }

    #[test]
    fn bounded_capture_queue_records_overflow_as_a_runtime_failure() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let failure = Arc::new(Mutex::new(None));
        let submitter = AudioSubmitter {
            sender,
            failure: Arc::clone(&failure),
        };
        submitter.submit_pcm(0, vec![0.0, 0.0]);
        submitter.submit_pcm(1, vec![0.0, 0.0]);
        assert_eq!(
            failure.lock().unwrap().as_deref(),
            Some("audio capture queue is full")
        );
    }

    #[test]
    fn first_runtime_failure_is_preserved_for_clean_session_termination() {
        let failure = Mutex::new(None);
        store_failure(&failure, "encoder stopped");
        store_failure(&failure, "later PipeWire error");
        assert_eq!(
            failure.into_inner().unwrap().as_deref(),
            Some("encoder stopped")
        );
    }

    #[test]
    fn startup_stream_errors_do_not_leak_into_runtime_failure_state() {
        let (startup_tx, startup_rx) = mpsc::channel();
        let mut startup = Some(startup_tx);
        let started = AtomicBool::new(false);
        let failure = Mutex::new(None);

        report_stream_error(&mut startup, &started, &failure, "no target node available");
        assert_eq!(
            startup_rx.recv().unwrap().unwrap_err(),
            "no target node available"
        );
        report_stream_error(&mut startup, &started, &failure, "disconnected");
        assert!(failure.into_inner().unwrap().is_none());
    }

    #[test]
    fn running_stream_errors_reach_runtime_failure_state() {
        let mut startup = None;
        let started = AtomicBool::new(true);
        let failure = Mutex::new(None);

        report_stream_error(&mut startup, &started, &failure, "disconnected");
        assert_eq!(
            failure.into_inner().unwrap().as_deref(),
            Some("PipeWire audio stream error: disconnected")
        );
    }
}
