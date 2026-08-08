use std::{
    collections::VecDeque,
    ffi::c_void,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use screencapturekit::prelude::*;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
pub const BITRATE: u32 = 192_000;
pub const ACCESS_UNIT_SAMPLES: usize = 1_024;
pub const RTP_PAYLOAD_TYPE: u8 = 97;

const AUDIO_QUEUE_CAPACITY: usize = 64;
const AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED: u32 = 1 << 5;
const MAX_CONTINUOUS_SILENCE_SAMPLES: u64 = SAMPLE_RATE as u64 * 2;

pub struct PreparedAudioEncoder(AacEncoder);

pub fn prepare() -> Result<PreparedAudioEncoder> {
    AacEncoder::new().map(PreparedAudioEncoder)
}

#[derive(Default)]
pub struct MediaClock {
    origin: Mutex<Option<CMTime>>,
}

impl MediaClock {
    pub fn ticks(&self, time: CMTime, timescale: u64) -> Option<u64> {
        if !time.is_valid() || time.value < 0 || time.timescale <= 0 {
            return None;
        }
        let origin = {
            let mut origin = self
                .origin
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *origin.get_or_insert(time)
        };
        let delta = i128::from(time.value)
            .checked_mul(i128::from(origin.timescale))?
            .checked_sub(i128::from(origin.value).checked_mul(i128::from(time.timescale))?)?;
        if delta < 0 {
            return Some(0);
        }
        let denominator = i128::from(time.timescale).checked_mul(i128::from(origin.timescale))?;
        let ticks = delta.checked_mul(i128::from(timescale))? / denominator;
        u64::try_from(ticks).ok()
    }
}

#[derive(Debug, Clone)]
pub struct EncodedAudioFrame {
    pub timestamp: u64,
    pub data: Vec<u8>,
}

enum AudioCommand {
    Sample(CMSampleBuffer),
    AdvanceTo(u64),
}

#[derive(Clone)]
pub struct AudioSubmitter {
    sender: SyncSender<AudioCommand>,
    failure: Arc<Mutex<Option<String>>>,
}

impl AudioSubmitter {
    pub fn submit(&self, sample: &CMSampleBuffer) {
        self.try_send(
            AudioCommand::Sample(sample.clone()),
            "audio capture queue is full",
        );
    }

    pub fn advance_to(&self, timestamp: u64) {
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
                store_failure(&self.failure, "audio encoder worker stopped unexpectedly")
            }
        }
    }
}

pub struct AudioWorker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioWorker {
    pub fn start<F>(
        clock: Arc<MediaClock>,
        failure: Arc<Mutex<Option<String>>>,
        output: F,
    ) -> Result<(AudioSubmitter, Self)>
    where
        F: FnMut(EncodedAudioFrame) -> Result<()> + Send + 'static,
    {
        Self::start_prepared(prepare()?, clock, failure, output)
    }

    pub fn start_prepared<F>(
        encoder: PreparedAudioEncoder,
        clock: Arc<MediaClock>,
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
                let mut pipeline = AudioPipeline::new(clock, encoder);
                while !worker_stop.load(Ordering::SeqCst) {
                    let command = match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    let result = match command {
                        AudioCommand::Sample(sample) => pipeline.push_sample(&sample, &mut output),
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

    pub fn stop(mut self) -> Result<()> {
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
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            log::warn!("AAC encoder worker panicked during shutdown");
        }
    }
}

pub struct AudioFrameHandler {
    submitter: AudioSubmitter,
}

impl AudioFrameHandler {
    pub fn new(submitter: AudioSubmitter) -> Self {
        Self { submitter }
    }
}

impl SCStreamOutputTrait for AudioFrameHandler {
    fn did_output_sample_buffer(
        &self,
        sample_buffer: CMSampleBuffer,
        output_type: SCStreamOutputType,
    ) {
        if output_type == SCStreamOutputType::Audio {
            self.submitter.submit(&sample_buffer);
        }
    }
}

struct AudioPipeline {
    clock: Arc<MediaClock>,
    encoder: AacEncoder,
    left: VecDeque<f32>,
    right: VecDeque<f32>,
    fifo_start: u64,
    next_sample: u64,
}

impl AudioPipeline {
    fn new(clock: Arc<MediaClock>, encoder: AacEncoder) -> Self {
        Self {
            clock,
            encoder,
            left: VecDeque::new(),
            right: VecDeque::new(),
            fifo_start: 0,
            next_sample: 0,
        }
    }

    fn push_sample<F>(&mut self, sample: &CMSampleBuffer, output: &mut F) -> Result<()>
    where
        F: FnMut(EncodedAudioFrame) -> Result<()>,
    {
        sample
            .make_data_ready()
            .map_err(|status| anyhow!("CoreMedia could not ready audio data ({status})"))?;
        let description = sample
            .format_description()
            .ok_or_else(|| anyhow!("captured audio has no format description"))?;
        let sample_rate = description.audio_sample_rate().unwrap_or_default();
        let channels = description.audio_channel_count().unwrap_or_default();
        let bits = description.audio_bits_per_channel().unwrap_or_default();
        if (sample_rate - f64::from(SAMPLE_RATE)).abs() > 0.5
            || channels != CHANNELS
            || bits != 32
            || !description.audio_is_float()
            || description.audio_is_big_endian()
        {
            bail!(
                "unsupported captured audio format: {sample_rate} Hz, {channels} channels, {bits}-bit{}",
                if description.audio_is_float() {
                    " float"
                } else {
                    " integer"
                }
            );
        }

        let frame_count = usize::try_from(sample.num_samples())
            .context("captured audio sample count was invalid")?;
        let buffers = sample
            .audio_buffer_list()
            .ok_or_else(|| anyhow!("captured audio has no PCM buffers"))?;
        let non_interleaved = description
            .audio_format_flags()
            .is_some_and(|flags| flags & AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED != 0);
        let (mut left, mut right) = if non_interleaved && buffers.num_buffers() == 2 {
            (
                decode_f32(buffers.get(0).unwrap().data(), frame_count)?,
                decode_f32(buffers.get(1).unwrap().data(), frame_count)?,
            )
        } else if !non_interleaved && buffers.num_buffers() == 1 {
            decode_interleaved_stereo(buffers.get(0).unwrap().data(), frame_count)?
        } else {
            bail!(
                "unsupported captured audio buffer layout ({} buffers, non-interleaved={non_interleaved})",
                buffers.num_buffers()
            );
        };

        let timestamp = self
            .clock
            .ticks(
                sample.output_presentation_timestamp(),
                u64::from(SAMPLE_RATE),
            )
            .unwrap_or(self.next_sample);
        if timestamp > self.next_sample {
            self.push_silence(timestamp - self.next_sample, output)?;
        } else if timestamp < self.next_sample {
            let trim = usize::try_from((self.next_sample - timestamp).min(frame_count as u64))
                .unwrap_or(frame_count);
            left.drain(..trim);
            right.drain(..trim);
        }
        self.next_sample = self.next_sample.saturating_add(left.len() as u64);
        self.left.extend(left);
        self.right.extend(right);
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
            log::debug!(
                "skipping a {:.1}s desktop audio discontinuity instead of encoding it as silence",
                frames as f64 / f64::from(SAMPLE_RATE)
            );
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
            if let Some(data) = self.encoder.encode(&left, &right)? {
                output(EncodedAudioFrame { timestamp, data })?;
            }
        }
        Ok(())
    }
}

fn decode_f32(bytes: &[u8], frames: usize) -> Result<Vec<f32>> {
    let needed = frames
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| anyhow!("captured audio buffer is too large"))?;
    if bytes.len() < needed {
        bail!("captured audio buffer is truncated");
    }
    Ok(bytes[..needed]
        .chunks_exact(4)
        .map(|chunk| f32::from_ne_bytes(chunk.try_into().unwrap()))
        .collect())
}

fn decode_interleaved_stereo(bytes: &[u8], frames: usize) -> Result<(Vec<f32>, Vec<f32>)> {
    let values = decode_f32(bytes, frames.saturating_mul(2))?;
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    for pair in values.chunks_exact(2) {
        left.push(pair[0]);
        right.push(pair[1]);
    }
    Ok((left, right))
}

fn store_failure(failure: &Mutex<Option<String>>, message: &str) {
    let mut failure = failure.lock().unwrap_or_else(|error| error.into_inner());
    if failure.is_none() {
        *failure = Some(message.to_owned());
    }
}

#[repr(C)]
struct NativeAacEncoder(c_void);

unsafe extern "C" {
    fn cast_aac_encoder_create(
        sample_rate: u32,
        channels: u32,
        bitrate: u32,
        maximum_packet_size: *mut u32,
    ) -> *mut NativeAacEncoder;
    fn cast_aac_encoder_encode(
        encoder: *mut NativeAacEncoder,
        left: *const f32,
        right: *const f32,
        frames: u32,
        output: *mut u8,
        output_capacity: u32,
        output_length: *mut u32,
    ) -> i32;
    fn cast_aac_encoder_destroy(encoder: *mut NativeAacEncoder);
}

struct AacEncoder {
    encoder: *mut NativeAacEncoder,
    output: Vec<u8>,
}

unsafe impl Send for AacEncoder {}

impl AacEncoder {
    fn new() -> Result<Self> {
        let mut maximum_packet_size = 0;
        let encoder = unsafe {
            cast_aac_encoder_create(SAMPLE_RATE, CHANNELS, BITRATE, &mut maximum_packet_size)
        };
        if encoder.is_null() || maximum_packet_size == 0 {
            bail!("AudioToolbox could not create the AAC-LC encoder");
        }
        Ok(Self {
            encoder,
            output: vec![0; maximum_packet_size as usize],
        })
    }

    fn encode(
        &mut self,
        left: &[f32; ACCESS_UNIT_SAMPLES],
        right: &[f32; ACCESS_UNIT_SAMPLES],
    ) -> Result<Option<Vec<u8>>> {
        let mut output_length = 0;
        let status = unsafe {
            cast_aac_encoder_encode(
                self.encoder,
                left.as_ptr(),
                right.as_ptr(),
                ACCESS_UNIT_SAMPLES as u32,
                self.output.as_mut_ptr(),
                self.output.len() as u32,
                &mut output_length,
            )
        };
        if status != 0 {
            bail!("AudioToolbox AAC encoding failed ({status})");
        }
        if output_length == 0 {
            return Ok(None);
        }
        let raw = &self.output[..output_length as usize];
        let mut framed = adts_header(raw.len())?;
        framed.extend_from_slice(raw);
        Ok(Some(framed))
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        unsafe { cast_aac_encoder_destroy(self.encoder) };
    }
}

fn adts_header(payload_length: usize) -> Result<Vec<u8>> {
    let frame_length = payload_length
        .checked_add(7)
        .ok_or_else(|| anyhow!("AAC packet length overflow"))?;
    if frame_length > 0x1fff {
        bail!("AAC packet is too large for ADTS");
    }
    let profile = 1_u8; // AAC Low Complexity, minus one as encoded by ADTS.
    let frequency_index = 3_u8; // 48 kHz.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adts_header_describes_aac_lc_stereo_at_48_khz() {
        let header = adts_header(100).unwrap();
        assert_eq!(&header[..2], &[0xff, 0xf1]);
        assert_eq!((header[2] >> 2) & 0xf, 3);
        assert_eq!(((header[2] & 1) << 2) | (header[3] >> 6), 2);
        let length = (usize::from(header[3] & 3) << 11)
            | (usize::from(header[4]) << 3)
            | usize::from(header[5] >> 5);
        assert_eq!(length, 107);
    }

    #[test]
    fn media_clock_uses_one_epoch_across_timebases() {
        let clock = MediaClock::default();
        assert_eq!(clock.ticks(CMTime::new(10, 1), 90_000), Some(0));
        assert_eq!(clock.ticks(CMTime::new(21, 2), 48_000), Some(24_000));
        assert_eq!(clock.ticks(CMTime::new(11, 1), 90_000), Some(90_000));
    }
}
