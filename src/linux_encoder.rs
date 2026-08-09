#![allow(dead_code)] // Wired into portal capture by CAS-26.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

use anyhow::{Context, Result, anyhow, bail};
use ffmpeg::{Dictionary, Packet, Rational, codec, encoder, frame, software::scaling};
use ffmpeg_next as ffmpeg;

use crate::{
    desktop::VideoEncoderControl,
    media::{H264Provider, VaapiFrames, select_linux_encoder},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RawPixelFormat {
    Bgra,
    Bgrx,
    Rgba,
    Rgbx,
}

impl RawPixelFormat {
    fn ffmpeg(self) -> ffmpeg::format::Pixel {
        match self {
            Self::Bgra => ffmpeg::format::Pixel::BGRA,
            Self::Bgrx => ffmpeg::format::Pixel::BGRZ,
            Self::Rgba => ffmpeg::format::Pixel::RGBA,
            Self::Rgbx => ffmpeg::format::Pixel::RGBZ,
        }
    }
}

pub(crate) struct RawVideoFrame<'a> {
    pub(crate) data: &'a [u8],
    pub(crate) stride: usize,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) format: RawPixelFormat,
    pub(crate) timestamp: u64,
}

#[derive(Debug)]
pub(crate) struct EncodedPacket {
    pub(crate) data: Vec<u8>,
    pub(crate) timestamp: u64,
    pub(crate) keyframe: bool,
}

pub(crate) struct LinuxEncoderControl {
    bitrate: AtomicU32,
    generation: AtomicU64,
    force_keyframe: AtomicBool,
}

impl LinuxEncoderControl {
    fn new(bitrate: u32) -> Self {
        Self {
            bitrate: AtomicU32::new(bitrate),
            generation: AtomicU64::new(0),
            force_keyframe: AtomicBool::new(true),
        }
    }
}

impl VideoEncoderControl for LinuxEncoderControl {
    fn set_bitrate(&self, bitrate: u32) -> Result<()> {
        if bitrate == 0 {
            bail!("encoder bitrate must be greater than zero");
        }
        if self.bitrate.swap(bitrate, Ordering::SeqCst) != bitrate {
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.force_keyframe.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    fn force_keyframe(&self) -> Result<()> {
        self.force_keyframe.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct OpenEncoder {
    encoder: encoder::video::Encoder,
    scaler: scaling::Context,
    input_format: ffmpeg::format::Pixel,
    source_width: u32,
    source_height: u32,
    output_format: ffmpeg::format::Pixel,
    vaapi: Option<VaapiFrames>,
}

pub(crate) struct LinuxVideoEncoder {
    provider: H264Provider,
    width: u32,
    height: u32,
    fps: u32,
    control: Arc<LinuxEncoderControl>,
    applied_generation: u64,
    open: OpenEncoder,
    last_timestamp: Option<u64>,
}

impl LinuxVideoEncoder {
    pub(crate) fn new(
        provider: H264Provider,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        input_format: RawPixelFormat,
    ) -> Result<Self> {
        if width == 0 || height == 0 || fps == 0 || bitrate == 0 {
            bail!("encoder dimensions, frame rate, and bitrate must be greater than zero");
        }
        let control = Arc::new(LinuxEncoderControl::new(bitrate));
        let open = open_encoder(
            provider,
            width,
            height,
            width,
            height,
            fps,
            bitrate,
            input_format.ffmpeg(),
        )?;
        Ok(Self {
            provider,
            width,
            height,
            fps,
            control,
            applied_generation: 0,
            open,
            last_timestamp: None,
        })
    }

    pub(crate) fn control(&self) -> Arc<LinuxEncoderControl> {
        Arc::clone(&self.control)
    }

    pub(crate) const fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub(crate) const fn input_format(&self) -> RawPixelFormat {
        match self.open.input_format {
            ffmpeg::format::Pixel::BGRA => RawPixelFormat::Bgra,
            ffmpeg::format::Pixel::BGRZ => RawPixelFormat::Bgrx,
            ffmpeg::format::Pixel::RGBA => RawPixelFormat::Rgba,
            ffmpeg::format::Pixel::RGBZ => RawPixelFormat::Rgbx,
            _ => unreachable!(),
        }
    }

    pub(crate) fn encode(&mut self, source: RawVideoFrame<'_>) -> Result<Vec<EncodedPacket>> {
        if source.width == 0 || source.height == 0 {
            bail!("captured frame dimensions must be greater than zero");
        }
        let minimum_stride = usize::try_from(source.width)?.saturating_mul(4);
        let required = source
            .stride
            .checked_mul(usize::try_from(source.height)?)
            .ok_or_else(|| anyhow!("captured frame size overflowed"))?;
        if source.stride < minimum_stride || source.data.len() < required {
            bail!("captured frame buffer is shorter than its dimensions and stride require");
        }

        let generation = self.control.generation.load(Ordering::SeqCst);
        let input_format = source.format.ffmpeg();
        if generation != self.applied_generation
            || input_format != self.open.input_format
            || source.width != self.open.source_width
            || source.height != self.open.source_height
        {
            let bitrate = self.control.bitrate.load(Ordering::SeqCst);
            self.open = open_encoder(
                self.provider,
                self.width,
                self.height,
                source.width,
                source.height,
                self.fps,
                bitrate,
                input_format,
            )?;
            self.applied_generation = generation;
            self.control.force_keyframe.store(true, Ordering::SeqCst);
        }

        let timestamp = self.last_timestamp.map_or(source.timestamp, |last| {
            source.timestamp.max(last.saturating_add(1))
        });
        self.last_timestamp = Some(timestamp);
        let mut input = frame::Video::new(input_format, source.width, source.height);
        let destination_stride = input.stride(0);
        let destination = input.data_mut(0);
        for row in 0..usize::try_from(source.height)? {
            let source_start = row * source.stride;
            let destination_start = row * destination_stride;
            destination[destination_start..destination_start + minimum_stride]
                .copy_from_slice(&source.data[source_start..source_start + minimum_stride]);
        }
        let mut converted = frame::Video::new(self.open.output_format, self.width, self.height);
        self.open
            .scaler
            .run(&input, &mut converted)
            .context("could not convert the captured PipeWire frame")?;
        converted.set_pts(Some(i64::try_from(timestamp)?));
        if self.control.force_keyframe.swap(false, Ordering::SeqCst) {
            converted.set_kind(ffmpeg::picture::Type::I);
        } else {
            converted.set_kind(ffmpeg::picture::Type::None);
        }

        if let Some(vaapi) = &self.open.vaapi {
            let uploaded = vaapi.upload(&converted)?;
            self.open
                .encoder
                .send_frame(&uploaded)
                .context("VA-API rejected a captured video frame")?;
        } else {
            self.open
                .encoder
                .send_frame(&converted)
                .context("Linux H.264 encoder rejected a captured video frame")?;
        }
        drain_packets(&mut self.open.encoder)
    }
}

#[allow(clippy::too_many_arguments)]
fn open_encoder(
    provider: H264Provider,
    width: u32,
    height: u32,
    source_width: u32,
    source_height: u32,
    fps: u32,
    bitrate: u32,
    input_format: ffmpeg::format::Pixel,
) -> Result<OpenEncoder> {
    let name = select_linux_encoder(provider)?;
    let codec = encoder::find_by_name(name)
        .ok_or_else(|| anyhow!("selected Linux encoder {name} disappeared"))?;
    let advertised_formats = codec
        .video()
        .context("selected Linux H.264 encoder is not a video encoder")?
        .formats()
        .map(|formats| formats.collect::<Vec<_>>())
        .unwrap_or_default();
    let output_format =
        if name == "h264_vaapi" || advertised_formats.contains(&ffmpeg::format::Pixel::NV12) {
            ffmpeg::format::Pixel::NV12
        } else {
            ffmpeg::format::Pixel::YUV420P
        };
    let mut encoder = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .context("could not configure the Linux H.264 encoder")?;
    encoder.set_width(width);
    encoder.set_height(height);
    encoder.set_format(if name == "h264_vaapi" {
        ffmpeg::format::Pixel::VAAPI
    } else {
        output_format
    });
    encoder.set_bit_rate(bitrate as usize);
    encoder.set_max_bit_rate(bitrate as usize);
    encoder.set_max_b_frames(0);
    encoder.set_gop(fps);
    encoder.set_frame_rate(Some(Rational::new(fps as i32, 1)));
    encoder.set_time_base(Rational::new(1, 90_000));
    let vaapi = (name == "h264_vaapi")
        .then(|| VaapiFrames::attach(&mut encoder, width, height))
        .transpose()?;
    let mut options = Dictionary::new();
    // Cast receivers are offered Baseline profile in both RTSP and HLS. Keep the
    // encoded SPS consistent with that offer; in particular, OpenH264 rejects
    // FFmpeg's numeric constrained-baseline profile but accepts baseline.
    options.set("profile", "baseline");
    options.set("repeat_headers", "1");
    if name == "h264_nvenc" {
        options.set("preset", "p1");
        options.set("tune", "ull");
        options.set("zerolatency", "1");
    }
    if name == "libopenh264" {
        options.set("allow_skip_frames", "1");
    }
    let encoder = encoder
        .open_as_with(codec, options)
        .with_context(|| format!("could not open Linux H.264 provider {name}"))?;
    let scaler = scaling::Context::get(
        input_format,
        source_width,
        source_height,
        output_format,
        width,
        height,
        scaling::Flags::FAST_BILINEAR,
    )
    .context("could not create the Linux desktop video converter")?;
    Ok(OpenEncoder {
        encoder,
        scaler,
        input_format,
        source_width,
        source_height,
        output_format,
        vaapi,
    })
}

fn drain_packets(encoder: &mut encoder::video::Encoder) -> Result<Vec<EncodedPacket>> {
    let mut packets = Vec::new();
    loop {
        let mut packet = Packet::empty();
        match encoder.receive_packet(&mut packet) {
            Ok(()) => {
                let timestamp = packet.pts().or_else(|| packet.dts()).unwrap_or(0).max(0) as u64;
                packets.push(EncodedPacket {
                    data: packet.data().unwrap_or_default().to_vec(),
                    timestamp,
                    keyframe: packet.is_key(),
                });
            }
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => break,
            Err(ffmpeg::Error::Eof) => break,
            Err(error) => return Err(error).context("Linux H.264 encoding failed"),
        }
    }
    Ok(packets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_requests_validate_bitrate_and_coalesce_restarts() {
        let control = LinuxEncoderControl::new(6_000_000);
        assert!(control.set_bitrate(0).is_err());
        control.set_bitrate(4_800_000).unwrap();
        control.set_bitrate(4_800_000).unwrap();
        assert_eq!(control.generation.load(Ordering::SeqCst), 1);
        assert!(control.force_keyframe.load(Ordering::SeqCst));
    }

    #[test]
    fn raw_frame_formats_map_to_the_expected_ffmpeg_pixels() {
        assert_eq!(RawPixelFormat::Bgra.ffmpeg(), ffmpeg::format::Pixel::BGRA);
        assert_eq!(RawPixelFormat::Bgrx.ffmpeg(), ffmpeg::format::Pixel::BGRZ);
        assert_eq!(RawPixelFormat::Rgba.ffmpeg(), ffmpeg::format::Pixel::RGBA);
        assert_eq!(RawPixelFormat::Rgbx.ffmpeg(), ffmpeg::format::Pixel::RGBZ);
    }

    #[test]
    fn openh264_streaming_control_smoke_when_module_is_available() {
        if crate::setup::find_compatible().unwrap().is_none() {
            return;
        }
        let pixels = vec![0_u8; 64 * 64 * 4];
        let mut encoder = LinuxVideoEncoder::new(
            H264Provider::Openh264,
            64,
            64,
            30,
            1_000_000,
            RawPixelFormat::Bgra,
        )
        .unwrap();
        let control = encoder.control();
        let first = encoder
            .encode(RawVideoFrame {
                data: &pixels,
                stride: 64 * 4,
                width: 64,
                height: 64,
                format: RawPixelFormat::Bgra,
                timestamp: 0,
            })
            .unwrap();
        assert!(first.iter().any(|packet| packet.keyframe));
        control.set_bitrate(800_000).unwrap();
        let restarted = encoder
            .encode(RawVideoFrame {
                data: &pixels,
                stride: 64 * 4,
                width: 64,
                height: 64,
                format: RawPixelFormat::Bgra,
                timestamp: 3_000,
            })
            .unwrap();
        assert!(restarted.iter().any(|packet| packet.keyframe));
        assert!(restarted.iter().all(|packet| packet.timestamp >= 3_000));

        let resized_pixels = vec![0_u8; 80 * 48 * 4];
        let resized = encoder
            .encode(RawVideoFrame {
                data: &resized_pixels,
                stride: 80 * 4,
                width: 80,
                height: 48,
                format: RawPixelFormat::Bgra,
                timestamp: 6_000,
            })
            .unwrap();
        assert!(resized.iter().any(|packet| packet.keyframe));
        assert!(resized.iter().all(|packet| packet.timestamp >= 6_000));
    }
}
