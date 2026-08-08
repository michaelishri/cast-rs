use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, anyhow, bail};
use ffmpeg::{
    ChannelLayout, Dictionary, Packet, Rational, codec, decoder, encoder,
    format::{self},
    frame, media,
    software::{
        resampling::Context as ResamplingContext,
        scaling::{context::Context as ScalingContext, flag::Flags as ScalingFlags},
    },
    util::format::pixel::Pixel,
};
use ffmpeg_next as ffmpeg;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompatibilityMode {
    #[default]
    Auto,
    Never,
    Always,
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub container: String,
    pub duration: Option<f64>,
    pub video: VideoInfo,
    pub audio: Option<AudioInfo>,
}

#[derive(Clone, Debug)]
pub struct VideoInfo {
    pub stream_index: usize,
    pub codec: codec::Id,
    pub codec_name: String,
    pub width: u32,
    pub height: u32,
    pub frame_rate: Option<f64>,
    pub pixel_format: Pixel,
}

#[derive(Clone, Debug)]
pub struct AudioInfo {
    pub stream_index: usize,
    pub codec: codec::Id,
    pub codec_name: String,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerFamily {
    Mp4,
    WebM,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TranscodeTracks {
    pub video: bool,
    pub audio: bool,
}

impl TranscodeTracks {
    pub const fn all(has_audio: bool) -> Self {
        Self {
            video: true,
            audio: has_audio,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreparationPlan {
    Direct {
        content_type: String,
    },
    Remux {
        reason: String,
    },
    Transcode {
        reasons: Vec<String>,
        tracks: TranscodeTracks,
    },
}

impl PreparationPlan {
    pub fn description(&self) -> String {
        match self {
            Self::Direct { .. } => "direct playback".to_owned(),
            Self::Remux { reason } => format!("lossless MP4 remux ({reason})"),
            Self::Transcode { reasons, tracks } => match (tracks.video, tracks.audio) {
                (true, true) => {
                    format!("H.264/AAC compatibility transcode ({})", reasons.join(", "))
                }
                (true, false) => {
                    format!("H.264 video compatibility transcode ({})", reasons.join(", "))
                }
                (false, true) => format!(
                    "AAC audio transcode with H.264 video copy ({})",
                    reasons.join(", ")
                ),
                (false, false) => "lossless track copy".to_owned(),
            },
        }
    }
}

pub fn inspect(path: &Path) -> Result<MediaInfo> {
    ffmpeg::init().context("could not initialize the linked FFmpeg libraries")?;
    ffmpeg::log::set_level(ffmpeg::log::Level::Warning);
    let input = format::input(path)
        .with_context(|| format!("could not inspect media container {}", path.display()))?;
    let container = input.format().name().to_owned();
    let duration = (input.duration() > 0).then(|| input.duration() as f64 / 1_000_000.0);

    let video_stream = input
        .streams()
        .best(media::Type::Video)
        .ok_or_else(|| anyhow!("media file does not contain a video stream"))?;
    let video_parameters = video_stream.parameters();
    let video_codec = video_parameters.id();
    let video_decoder = codec::context::Context::from_parameters(video_parameters)
        .context("could not read video stream parameters")?
        .decoder()
        .video()
        .with_context(|| format!("no decoder is available for {} video", video_codec.name()))?;
    let rate = video_stream.avg_frame_rate();
    let frame_rate = (rate.denominator() != 0 && rate.numerator() > 0)
        .then(|| rate.numerator() as f64 / rate.denominator() as f64);
    let video = VideoInfo {
        stream_index: video_stream.index(),
        codec: video_codec,
        codec_name: video_codec.name().to_owned(),
        width: video_decoder.width(),
        height: video_decoder.height(),
        frame_rate,
        pixel_format: video_decoder.format(),
    };

    let audio = input
        .streams()
        .best(media::Type::Audio)
        .map(|stream| -> Result<AudioInfo> {
            let parameters = stream.parameters();
            let audio_codec = parameters.id();
            let decoder = codec::context::Context::from_parameters(parameters)
                .context("could not read audio stream parameters")?
                .decoder()
                .audio()
                .with_context(|| {
                    format!("no decoder is available for {} audio", audio_codec.name())
                })?;
            Ok(AudioInfo {
                stream_index: stream.index(),
                codec: audio_codec,
                codec_name: audio_codec.name().to_owned(),
                sample_rate: decoder.rate(),
                channels: decoder.channels(),
            })
        })
        .transpose()?;

    Ok(MediaInfo {
        container,
        duration,
        video,
        audio,
    })
}

pub fn plan(info: &MediaInfo, mode: CompatibilityMode) -> Result<PreparationPlan> {
    let family = container_family(&info.container);
    let direct = direct_content_type(info, family);
    match mode {
        CompatibilityMode::Always => Ok(PreparationPlan::Transcode {
            reasons: vec!["requested by --transcode=always".to_owned()],
            tracks: TranscodeTracks::all(info.audio.is_some()),
        }),
        CompatibilityMode::Never => direct
            .map(|content_type| PreparationPlan::Direct {
                content_type: content_type.to_owned(),
            })
            .ok_or_else(|| {
                anyhow!(
                    "{} video with {} audio in {} is not in the direct-play compatibility set and transcoding is disabled",
                    info.video.codec_name,
                    info.audio
                        .as_ref()
                        .map_or("no".to_owned(), |audio| audio.codec_name.clone()),
                    info.container
                )
            }),
        CompatibilityMode::Auto => {
            if let Some(content_type) = direct {
                return Ok(PreparationPlan::Direct {
                    content_type: content_type.to_owned(),
                });
            }
            if info.video.codec == codec::Id::H264
                && conservative_stream_properties(info)
                && info
                    .audio
                    .as_ref()
                    .is_none_or(|audio| audio.codec == codec::Id::AAC)
            {
                return Ok(PreparationPlan::Remux {
                    reason: format!("{} is not directly served", info.container),
                });
            }

            let transcode_video =
                info.video.codec != codec::Id::H264 || !conservative_video_properties(&info.video);
            let transcode_audio = info.audio.as_ref().is_some_and(|audio| {
                audio.codec != codec::Id::AAC || !conservative_audio_properties(audio)
            });
            let tracks = TranscodeTracks {
                video: transcode_video,
                audio: transcode_audio,
            };
            let mut reasons = Vec::new();
            if info.video.codec != codec::Id::H264 {
                reasons.push(format!("{} video", info.video.codec_name));
            } else if !conservative_video_properties(&info.video) {
                reasons.push("video dimensions or pixel format".to_owned());
            }
            if let Some(audio) = &info.audio
                && audio.codec != codec::Id::AAC
            {
                reasons.push(format!("{} audio", audio.codec_name));
            } else if let Some(audio) = &info.audio
                && !conservative_audio_properties(audio)
            {
                reasons.push("audio layout or sample rate".to_owned());
            }
            if reasons.is_empty() {
                reasons.push("container compatibility".to_owned());
            }
            Ok(PreparationPlan::Transcode { reasons, tracks })
        }
    }
}

fn direct_content_type(info: &MediaInfo, family: ContainerFamily) -> Option<&'static str> {
    let conservative_streams = conservative_stream_properties(info);
    match family {
        ContainerFamily::Mp4
            if info.video.codec == codec::Id::H264
                && conservative_streams
                && info
                    .audio
                    .as_ref()
                    .is_none_or(|audio| audio.codec == codec::Id::AAC) =>
        {
            Some("video/mp4")
        }
        ContainerFamily::WebM
            if matches!(info.video.codec, codec::Id::VP8 | codec::Id::VP9)
                && conservative_streams
                && info.audio.as_ref().is_none_or(|audio| {
                    matches!(audio.codec, codec::Id::VORBIS | codec::Id::OPUS)
                }) =>
        {
            Some("video/webm")
        }
        _ => None,
    }
}

fn conservative_stream_properties(info: &MediaInfo) -> bool {
    let video = conservative_video_properties(&info.video);
    let audio = info
        .audio
        .as_ref()
        .is_none_or(conservative_audio_properties);
    video && audio
}

fn conservative_video_properties(video: &VideoInfo) -> bool {
    video.width <= MAX_OUTPUT_WIDTH
        && video.height <= MAX_OUTPUT_HEIGHT
        && matches!(video.pixel_format, Pixel::YUV420P | Pixel::NV12)
}

fn conservative_audio_properties(audio: &AudioInfo) -> bool {
    audio.channels <= 2 && audio.sample_rate <= AUDIO_RATE
}

pub fn container_family(name: &str) -> ContainerFamily {
    let names = name.split(',');
    if names
        .clone()
        .any(|name| matches!(name, "mov" | "mp4" | "m4a" | "3gp" | "3g2" | "mj2"))
    {
        ContainerFamily::Mp4
    } else if names.into_iter().any(|name| name == "webm") {
        ContainerFamily::WebM
    } else {
        ContainerFamily::Other
    }
}

pub fn remux_to_mp4(
    input_path: &Path,
    output_path: &Path,
    info: &MediaInfo,
    cancelled: &AtomicBool,
) -> Result<()> {
    let mut input = format::input(input_path)
        .with_context(|| format!("could not open {} for remuxing", input_path.display()))?;
    let mut output = format::output_as(output_path, "mp4")
        .with_context(|| format!("could not create temporary MP4 {}", output_path.display()))?;
    let selected = [
        Some(info.video.stream_index),
        info.audio.as_ref().map(|audio| audio.stream_index),
    ];
    let mut mapping = vec![None; input.nb_streams() as usize];
    let mut input_time_bases = vec![Rational(0, 1); input.nb_streams() as usize];

    for input_index in selected.into_iter().flatten() {
        let stream = input
            .stream(input_index)
            .ok_or_else(|| anyhow!("selected input stream {input_index} disappeared"))?;
        let mut output_stream = output
            .add_stream(codec::encoder::find(codec::Id::None))
            .context("could not add MP4 stream")?;
        output_stream.set_parameters(stream.parameters());
        // Container-specific tags from Matroska/MOV are not valid in the new MP4.
        unsafe {
            (*output_stream.parameters().as_mut_ptr()).codec_tag = 0;
        }
        mapping[input_index] = Some(output_stream.index());
        input_time_bases[input_index] = stream.time_base();
    }

    output.set_metadata(input.metadata().to_owned());
    let mut options = ffmpeg::Dictionary::new();
    options.set("movflags", "+faststart");
    let unused = output
        .write_header_with(options)
        .context("could not write temporary MP4 header")?;
    if unused.iter().next().is_some() {
        log::debug!("MP4 muxer did not consume every header option");
    }
    drop(unused);
    let output_time_bases = output
        .streams()
        .map(|stream| stream.time_base())
        .collect::<Vec<_>>();

    for (stream, mut packet) in input.packets() {
        if cancelled.load(Ordering::SeqCst) {
            bail!("media preparation was cancelled");
        }
        let Some(output_index) = mapping[stream.index()] else {
            continue;
        };
        if packet.pts().is_none() || packet.dts().is_none() {
            bail!(
                "stream {} omits presentation or decode timestamps required for a safe MP4 remux",
                stream.index()
            );
        }
        packet.rescale_ts(
            input_time_bases[stream.index()],
            output_time_bases[output_index],
        );
        packet.set_position(-1);
        packet.set_stream(output_index);
        packet
            .write_interleaved(&mut output)
            .context("could not write a packet to the temporary MP4")?;
    }
    output
        .write_trailer()
        .context("could not finish the temporary MP4")?;
    Ok(())
}

const MAX_OUTPUT_WIDTH: u32 = 1920;
const MAX_OUTPUT_HEIGHT: u32 = 1080;
const VIDEO_BIT_RATE: usize = 6_000_000;
const AUDIO_BIT_RATE: usize = 192_000;
const AUDIO_RATE: u32 = 48_000;

struct VideoTranscoder {
    output_index: usize,
    input_time_base: Rational,
    decoder: decoder::Video,
    encoder: encoder::video::Encoder,
    scaler: ScalingContext,
    output_format: Pixel,
    output_width: u32,
    output_height: u32,
    frame_duration: i64,
    last_output_dts: Option<i64>,
}

impl VideoTranscoder {
    fn new(
        input_stream: &format::stream::Stream<'_>,
        output: &mut format::context::Output,
    ) -> Result<Self> {
        let decoder = codec::context::Context::from_parameters(input_stream.parameters())
            .context("could not create the video decoder")?
            .decoder()
            .video()
            .context("could not open the source video decoder")?;
        let hardware = encoder::find_by_name("h264_videotoolbox");
        let selected_encoder = hardware
            .or_else(|| encoder::find(codec::Id::H264))
            .ok_or_else(|| anyhow!("linked FFmpeg libraries do not provide an H.264 encoder"))?;
        let advertised_formats = selected_encoder
            .video()
            .context("selected H.264 encoder is not a video encoder")?
            .formats()
            .map(|formats| formats.collect::<Vec<_>>())
            .unwrap_or_default();
        let output_format = if advertised_formats.contains(&Pixel::NV12) {
            Pixel::NV12
        } else {
            Pixel::YUV420P
        };
        let (output_width, output_height) =
            compatible_dimensions(decoder.width(), decoder.height());
        let global_header = output
            .format()
            .flags()
            .contains(format::Flags::GLOBAL_HEADER);
        let mut output_stream = output
            .add_stream(selected_encoder)
            .context("could not add the H.264 output stream")?;
        let output_index = output_stream.index();
        let mut encoder = codec::context::Context::new_with_codec(selected_encoder)
            .encoder()
            .video()
            .context("could not configure the H.264 encoder")?;
        encoder.set_width(output_width);
        encoder.set_height(output_height);
        encoder.set_aspect_ratio(decoder.aspect_ratio());
        encoder.set_format(output_format);
        encoder.set_bit_rate(VIDEO_BIT_RATE);
        encoder.set_max_bit_rate(VIDEO_BIT_RATE);
        encoder.set_max_b_frames(0);
        let frame_rate = input_stream.avg_frame_rate();
        let frame_duration = if frame_rate.numerator() > 0
            && frame_rate.denominator() > 0
            && input_stream.time_base().numerator() > 0
        {
            (frame_rate.denominator() as f64 * input_stream.time_base().denominator() as f64
                / (frame_rate.numerator() as f64 * input_stream.time_base().numerator() as f64))
                .round()
                .max(1.0) as i64
        } else {
            1
        };
        if frame_rate.numerator() > 0 && frame_rate.denominator() > 0 {
            encoder.set_frame_rate(Some(frame_rate));
            encoder
                .set_gop(((frame_rate.numerator() / frame_rate.denominator()).max(1) * 2) as u32);
        } else {
            encoder.set_gop(60);
        }
        encoder.set_time_base(input_stream.time_base());
        output_stream.set_time_base(input_stream.time_base());
        let mut encoder_flags = codec::Flags::CLOSED_GOP;
        if global_header {
            encoder_flags.insert(codec::Flags::GLOBAL_HEADER);
        }
        encoder.set_flags(encoder_flags);
        let mut options = Dictionary::new();
        options.set("profile", "main");
        if hardware.is_some() {
            options.set("allow_sw", "1");
            options.set("realtime", "0");
        }
        let encoder = encoder
            .open_as_with(selected_encoder, options)
            .context("could not open the H.264 compatibility encoder")?;
        output_stream.set_parameters(&encoder);
        let scaler = ScalingContext::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            output_format,
            output_width,
            output_height,
            ScalingFlags::BILINEAR,
        )
        .context("could not create the video scaler")?;
        Ok(Self {
            output_index,
            input_time_base: input_stream.time_base(),
            decoder,
            encoder,
            scaler,
            output_format,
            output_width,
            output_height,
            frame_duration,
            last_output_dts: None,
        })
    }

    fn process_packet(
        &mut self,
        packet: &Packet,
        output: &mut format::context::Output,
        output_time_base: Rational,
    ) -> Result<()> {
        self.decoder
            .send_packet(packet)
            .context("video decoder rejected an input packet")?;
        self.drain_decoder(output, output_time_base)
    }

    fn drain_decoder(
        &mut self,
        output: &mut format::context::Output,
        output_time_base: Rational,
    ) -> Result<()> {
        let mut decoded = frame::Video::empty();
        loop {
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => {}
                Err(error) if receive_finished(error) => break,
                Err(error) => return Err(error).context("video decoding failed"),
            }
            if self.scaler.input().format != decoded.format()
                || self.scaler.input().width != decoded.width()
                || self.scaler.input().height != decoded.height()
            {
                self.scaler = ScalingContext::get(
                    decoded.format(),
                    decoded.width(),
                    decoded.height(),
                    self.output_format,
                    self.output_width,
                    self.output_height,
                    ScalingFlags::BILINEAR,
                )
                .context("source video format changed and the scaler could not be rebuilt")?;
            }
            let mut converted =
                frame::Video::new(self.output_format, self.output_width, self.output_height);
            self.scaler
                .run(&decoded, &mut converted)
                .context("could not convert a decoded video frame")?;
            converted.set_pts(decoded.timestamp());
            converted.set_kind(ffmpeg::picture::Type::None);
            self.encoder
                .send_frame(&converted)
                .context("H.264 encoder rejected a video frame")?;
            self.drain_encoder(output, output_time_base)?;
        }
        Ok(())
    }

    fn drain_encoder(
        &mut self,
        output: &mut format::context::Output,
        output_time_base: Rational,
    ) -> Result<()> {
        let mut packet = Packet::empty();
        loop {
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {}
                Err(error) if receive_finished(error) => break,
                Err(error) => return Err(error).context("H.264 encoding failed"),
            }
            if packet.duration() <= 0 {
                packet.set_duration(self.frame_duration);
            }
            packet.set_stream(self.output_index);
            packet.rescale_ts(self.input_time_base, output_time_base);
            if packet.duration() <= 0 {
                packet.set_duration(1);
            }
            let (dts, pts) = monotonic_packet_timestamps(
                packet.dts(),
                packet.pts(),
                &mut self.last_output_dts,
            );
            packet.set_dts(dts);
            packet.set_pts(pts);
            packet.set_position(-1);
            packet
                .write_interleaved(output)
                .context("could not write encoded H.264 video")?;
        }
        Ok(())
    }

    fn finish(
        &mut self,
        output: &mut format::context::Output,
        output_time_base: Rational,
    ) -> Result<()> {
        self.decoder
            .send_eof()
            .context("could not flush the video decoder")?;
        self.drain_decoder(output, output_time_base)?;
        self.encoder
            .send_eof()
            .context("could not flush the H.264 encoder")?;
        self.drain_encoder(output, output_time_base)
    }
}

fn monotonic_packet_timestamps(
    dts: Option<i64>,
    pts: Option<i64>,
    last_dts: &mut Option<i64>,
) -> (Option<i64>, Option<i64>) {
    let Some(mut dts) = dts else {
        return (None, pts);
    };
    let mut pts = pts;
    if let Some(previous_dts) = *last_dts
        && dts <= previous_dts
    {
        let minimum_dts = previous_dts.saturating_add(1);
        let adjustment = minimum_dts.saturating_sub(dts);
        dts = minimum_dts;
        pts = pts.map(|pts| pts.saturating_add(adjustment));
    }
    *last_dts = Some(dts);
    (Some(dts), pts)
}

struct AudioTranscoder {
    input_index: usize,
    output_index: usize,
    decoder: decoder::Audio,
    encoder: encoder::audio::Encoder,
    resampler: ResamplingContext,
    fifo: AudioFifo,
    decoder_time_base: Rational,
    next_pts: Option<i64>,
}

impl AudioTranscoder {
    fn new(
        input_stream: &format::stream::Stream<'_>,
        output: &mut format::context::Output,
    ) -> Result<Self> {
        let mut decoder = codec::context::Context::from_parameters(input_stream.parameters())
            .context("could not create the audio decoder")?
            .decoder()
            .audio()
            .context("could not open the source audio decoder")?;
        if decoder.channel_layout().is_empty() {
            decoder.set_channel_layout(ChannelLayout::default(i32::from(decoder.channels())));
        }
        let selected_encoder = encoder::find(codec::Id::AAC)
            .ok_or_else(|| anyhow!("linked FFmpeg libraries do not provide an AAC encoder"))?;
        let global_header = output
            .format()
            .flags()
            .contains(format::Flags::GLOBAL_HEADER);
        let mut output_stream = output
            .add_stream(selected_encoder)
            .context("could not add the AAC output stream")?;
        let output_index = output_stream.index();
        let mut encoder = codec::context::Context::new_with_codec(selected_encoder)
            .encoder()
            .audio()
            .context("could not configure the AAC encoder")?;
        encoder.set_rate(AUDIO_RATE as i32);
        encoder.set_channel_layout(ChannelLayout::STEREO);
        encoder.set_format(
            selected_encoder
                .audio()
                .context("selected AAC encoder is not an audio encoder")?
                .formats()
                .and_then(|mut formats| formats.next())
                .ok_or_else(|| anyhow!("AAC encoder does not advertise a sample format"))?,
        );
        encoder.set_bit_rate(AUDIO_BIT_RATE);
        encoder.set_max_bit_rate(AUDIO_BIT_RATE);
        encoder.set_time_base((1, AUDIO_RATE as i32));
        output_stream.set_time_base((1, AUDIO_RATE as i32));
        if global_header {
            encoder.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        let encoder = encoder
            .open_as(selected_encoder)
            .context("could not open the AAC compatibility encoder")?;
        output_stream.set_parameters(&encoder);
        let resampler = ResamplingContext::get(
            decoder.format(),
            decoder.channel_layout(),
            decoder.rate(),
            encoder.format(),
            encoder.channel_layout(),
            encoder.rate(),
        )
        .context("could not create the audio resampler")?;
        let fifo = AudioFifo::new(
            encoder.format(),
            encoder.channel_layout(),
            encoder.frame_size(),
        )?;
        let decoder_time_base = decoder.time_base();
        Ok(Self {
            input_index: input_stream.index(),
            output_index,
            decoder,
            encoder,
            resampler,
            fifo,
            decoder_time_base,
            next_pts: None,
        })
    }

    fn process_packet(
        &mut self,
        packet: &Packet,
        output: &mut format::context::Output,
        output_time_base: Rational,
    ) -> Result<()> {
        self.decoder
            .send_packet(packet)
            .context("audio decoder rejected an input packet")?;
        self.drain_decoder(output, output_time_base)
    }

    fn drain_decoder(
        &mut self,
        output: &mut format::context::Output,
        output_time_base: Rational,
    ) -> Result<()> {
        let mut decoded = frame::Audio::empty();
        loop {
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => {}
                Err(error) if receive_finished(error) => break,
                Err(error) => return Err(error).context("audio decoding failed"),
            }
            if self.next_pts.is_none() {
                self.next_pts = decoded.timestamp().map(|timestamp| {
                    (timestamp as f64 * self.decoder_time_base.numerator() as f64
                        / self.decoder_time_base.denominator() as f64
                        * AUDIO_RATE as f64)
                        .round() as i64
                });
            }
            let capacity = ((decoded.samples() as u64 * u64::from(AUDIO_RATE)
                / u64::from(self.decoder.rate().max(1)))
                + 256) as usize;
            let mut converted = frame::Audio::new(
                self.encoder.format(),
                capacity,
                self.encoder.channel_layout(),
            );
            self.resampler
                .run(&decoded, &mut converted)
                .context("could not resample a decoded audio frame")?;
            self.fifo.write(&mut converted)?;
            self.drain_fifo(output, output_time_base, false)?;
        }
        Ok(())
    }

    fn drain_fifo(
        &mut self,
        output: &mut format::context::Output,
        output_time_base: Rational,
        final_frame: bool,
    ) -> Result<()> {
        let frame_size = self.encoder.frame_size() as usize;
        while self.fifo.len() >= frame_size || (final_frame && self.fifo.len() > 0) {
            let samples = frame_size.min(self.fifo.len());
            let mut filtered = frame::Audio::new(
                self.encoder.format(),
                samples,
                self.encoder.channel_layout(),
            );
            filtered.set_rate(AUDIO_RATE);
            self.fifo.read(&mut filtered)?;
            let pts = self.next_pts.unwrap_or(0);
            filtered.set_pts(Some(pts));
            self.next_pts = Some(pts + samples as i64);
            self.encoder
                .send_frame(&filtered)
                .context("AAC encoder rejected an audio frame")?;
            self.drain_encoder(output, output_time_base)?;
        }
        Ok(())
    }

    fn drain_encoder(
        &mut self,
        output: &mut format::context::Output,
        output_time_base: Rational,
    ) -> Result<()> {
        let mut packet = Packet::empty();
        loop {
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {}
                Err(error) if receive_finished(error) => break,
                Err(error) => return Err(error).context("AAC encoding failed"),
            }
            packet.set_stream(self.output_index);
            packet.rescale_ts((1, AUDIO_RATE as i32), output_time_base);
            packet.set_position(-1);
            packet
                .write_interleaved(output)
                .context("could not write encoded AAC audio")?;
        }
        Ok(())
    }

    fn finish(
        &mut self,
        output: &mut format::context::Output,
        output_time_base: Rational,
    ) -> Result<()> {
        self.decoder
            .send_eof()
            .context("could not flush the audio decoder")?;
        self.drain_decoder(output, output_time_base)?;
        loop {
            let mut converted = frame::Audio::new(
                self.encoder.format(),
                self.encoder.frame_size() as usize,
                self.encoder.channel_layout(),
            );
            let delay = self
                .resampler
                .flush(&mut converted)
                .context("could not flush the audio resampler")?;
            if converted.samples() > 0 {
                self.fifo.write(&mut converted)?;
            }
            if delay.is_none() {
                break;
            }
        }
        self.drain_fifo(output, output_time_base, true)?;
        self.encoder
            .send_eof()
            .context("could not flush the AAC encoder")?;
        self.drain_encoder(output, output_time_base)
    }
}

struct AudioFifo {
    pointer: *mut ffmpeg::ffi::AVAudioFifo,
}

impl AudioFifo {
    fn new(format: ffmpeg::format::Sample, layout: ChannelLayout, frame_size: u32) -> Result<Self> {
        let sample_format: ffmpeg::ffi::AVSampleFormat = format.into();
        let pointer = unsafe {
            ffmpeg::ffi::av_audio_fifo_alloc(
                sample_format,
                layout.channels(),
                frame_size.max(1) as i32 * 2,
            )
        };
        if pointer.is_null() {
            bail!("could not allocate the audio sample FIFO");
        }
        Ok(Self { pointer })
    }

    fn len(&self) -> usize {
        unsafe { ffmpeg::ffi::av_audio_fifo_size(self.pointer).max(0) as usize }
    }

    fn write(&mut self, frame: &mut frame::Audio) -> Result<()> {
        let samples = frame.samples() as i32;
        let written = unsafe {
            ffmpeg::ffi::av_audio_fifo_write(
                self.pointer,
                (*frame.as_mut_ptr()).extended_data.cast(),
                samples,
            )
        };
        if written != samples {
            bail!("could not buffer resampled audio: expected {samples} samples, wrote {written}");
        }
        Ok(())
    }

    fn read(&mut self, frame: &mut frame::Audio) -> Result<()> {
        let samples = frame.samples() as i32;
        let read = unsafe {
            ffmpeg::ffi::av_audio_fifo_read(
                self.pointer,
                (*frame.as_mut_ptr()).extended_data.cast(),
                samples,
            )
        };
        if read != samples {
            bail!("could not read buffered audio: expected {samples} samples, read {read}");
        }
        Ok(())
    }
}

impl Drop for AudioFifo {
    fn drop(&mut self) {
        unsafe { ffmpeg::ffi::av_audio_fifo_free(self.pointer) };
    }
}

fn receive_finished(error: ffmpeg::Error) -> bool {
    matches!(
        error,
        ffmpeg::Error::Eof
            | ffmpeg::Error::Other {
                errno: ffmpeg::error::EAGAIN
            }
    )
}

struct CopiedStream {
    input_index: usize,
    output_index: usize,
    input_time_base: Rational,
    last_output_dts: Option<i64>,
    next_missing_dts: Option<i64>,
    pending_packets: Vec<Packet>,
}

impl CopiedStream {
    fn new(
        input_stream: &format::stream::Stream<'_>,
        output: &mut format::context::Output,
        description: &str,
    ) -> Result<Self> {
        let mut output_stream = output
            .add_stream(codec::encoder::find(codec::Id::None))
            .with_context(|| format!("could not add the copied {description} stream"))?;
        output_stream.set_parameters(input_stream.parameters());
        unsafe {
            (*output_stream.parameters().as_mut_ptr()).codec_tag = 0;
        }
        Ok(Self {
            input_index: input_stream.index(),
            output_index: output_stream.index(),
            input_time_base: input_stream.time_base(),
            last_output_dts: None,
            next_missing_dts: None,
            pending_packets: Vec::new(),
        })
    }

    fn write_packet(
        &mut self,
        mut packet: Packet,
        output: &mut format::context::Output,
        output_time_base: Rational,
        description: &str,
    ) -> Result<()> {
        if packet.pts().is_none() {
            bail!("{description} stream omits presentation timestamps required for track copying");
        }
        packet.rescale_ts(self.input_time_base, output_time_base);
        if packet.duration() <= 0 {
            packet.set_duration(1);
        }
        if packet.dts().is_none() {
            if let Some(dts) = self.next_missing_dts {
                packet.set_dts(Some(dts));
            } else {
                self.pending_packets.push(packet);
                return Ok(());
            }
        }
        if !self.pending_packets.is_empty() {
            let mut dts = packet
                .dts()
                .ok_or_else(|| anyhow!("copied {description} packet lost its decode timestamp"))?;
            for pending in self.pending_packets.iter_mut().rev() {
                dts = dts.saturating_sub(pending.duration().max(1));
                pending.set_dts(Some(dts));
            }
            let pending_packets = std::mem::take(&mut self.pending_packets);
            for pending in pending_packets {
                self.write_rescaled_packet(pending, output, description)?;
            }
        }
        self.write_rescaled_packet(packet, output, description)
    }

    fn write_rescaled_packet(
        &mut self,
        mut packet: Packet,
        output: &mut format::context::Output,
        description: &str,
    ) -> Result<()> {
        let (dts, pts) = monotonic_packet_timestamps(
            packet.dts(),
            packet.pts(),
            &mut self.last_output_dts,
        );
        packet.set_dts(dts);
        packet.set_pts(pts);
        self.next_missing_dts = dts.map(|dts| dts.saturating_add(packet.duration().max(1)));
        packet.set_position(-1);
        packet.set_stream(self.output_index);
        packet
            .write_interleaved(output)
            .with_context(|| format!("could not write copied {description}"))
    }

    fn finish_pending(
        &mut self,
        output: &mut format::context::Output,
        description: &str,
    ) -> Result<()> {
        let Some(first_pts) = self.pending_packets.first().and_then(Packet::pts) else {
            return Ok(());
        };
        let mut dts = first_pts;
        let pending_packets = std::mem::take(&mut self.pending_packets);
        for mut packet in pending_packets {
            packet.set_dts(Some(dts));
            dts = dts.saturating_add(packet.duration().max(1));
            self.write_rescaled_packet(packet, output, description)?;
        }
        Ok(())
    }
}

pub fn transcode_to_mp4(
    input_path: &Path,
    output_path: &Path,
    info: &MediaInfo,
    cancelled: &AtomicBool,
    progress: impl FnMut(f64),
) -> Result<()> {
    transcode_to_mp4_with_tracks(
        input_path,
        output_path,
        info,
        TranscodeTracks::all(info.audio.is_some()),
        cancelled,
        progress,
    )
}

pub fn transcode_to_mp4_with_tracks(
    input_path: &Path,
    output_path: &Path,
    info: &MediaInfo,
    tracks: TranscodeTracks,
    cancelled: &AtomicBool,
    progress: impl FnMut(f64),
) -> Result<()> {
    let mut output = format::output_as(output_path, "mp4")
        .with_context(|| format!("could not create temporary MP4 {}", output_path.display()))?;
    let mut options = Dictionary::new();
    options.set("movflags", "+faststart");
    transcode_to_output(
        input_path,
        info,
        tracks,
        cancelled,
        progress,
        &mut output,
        options,
    )
}

pub fn transcode_to_hls(
    input_path: &Path,
    playlist_path: &Path,
    segment_pattern: &Path,
    info: &MediaInfo,
    cancelled: &AtomicBool,
    progress: impl FnMut(f64),
) -> Result<()> {
    transcode_to_hls_with_tracks(
        input_path,
        playlist_path,
        segment_pattern,
        info,
        TranscodeTracks::all(info.audio.is_some()),
        cancelled,
        progress,
    )
}

pub fn transcode_to_hls_with_tracks(
    input_path: &Path,
    playlist_path: &Path,
    segment_pattern: &Path,
    info: &MediaInfo,
    tracks: TranscodeTracks,
    cancelled: &AtomicBool,
    progress: impl FnMut(f64),
) -> Result<()> {
    let mut output = format::output_as(playlist_path, "hls").with_context(|| {
        format!(
            "could not create incremental HLS playlist {}",
            playlist_path.display()
        )
    })?;
    let segment_pattern = segment_pattern
        .to_str()
        .ok_or_else(|| anyhow!("temporary HLS path is not valid UTF-8"))?;
    let mut options = Dictionary::new();
    options.set("hls_segment_type", "fmp4");
    options.set("hls_time", "2");
    options.set("hls_list_size", "0");
    options.set("hls_playlist_type", "event");
    options.set("hls_fmp4_init_filename", "init.mp4");
    options.set("hls_segment_filename", segment_pattern);
    options.set(
        "hls_flags",
        if tracks.video {
            "independent_segments+temp_file"
        } else {
            "temp_file"
        },
    );
    transcode_to_output(
        input_path,
        info,
        tracks,
        cancelled,
        progress,
        &mut output,
        options,
    )
}

fn transcode_to_output(
    input_path: &Path,
    info: &MediaInfo,
    tracks: TranscodeTracks,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(f64),
    output: &mut format::context::Output,
    options: Dictionary<'_>,
) -> Result<()> {
    let mut input = format::input(input_path)
        .with_context(|| format!("could not open {} for transcoding", input_path.display()))?;
    let video_stream = input
        .stream(info.video.stream_index)
        .ok_or_else(|| anyhow!("selected video stream disappeared"))?;
    let video_input_time_base = video_stream.time_base();
    let (mut video, mut copied_video) = if tracks.video {
        (Some(VideoTranscoder::new(&video_stream, output)?), None)
    } else {
        (
            None,
            Some(CopiedStream::new(&video_stream, output, "H.264 video")?),
        )
    };
    let (mut audio, mut copied_audio) = match &info.audio {
        Some(audio) => {
            let stream = input
                .stream(audio.stream_index)
                .ok_or_else(|| anyhow!("selected audio stream disappeared"))?;
            if tracks.audio {
                (Some(AudioTranscoder::new(&stream, output)?), None)
            } else {
                (None, Some(CopiedStream::new(&stream, output, "AAC audio")?))
            }
        }
        None => (None, None),
    };

    output.set_metadata(input.metadata().to_owned());
    output
        .write_header_with(options)
        .context("could not write the compatibility media header")?;
    let output_time_bases = output
        .streams()
        .map(|stream| stream.time_base())
        .collect::<Vec<_>>();
    let mut last_progress = -1.0;

    for (stream, packet) in input.packets() {
        if cancelled.load(Ordering::SeqCst) {
            bail!("media preparation was cancelled");
        }
        if stream.index() == info.video.stream_index {
            if let Some(timestamp) = packet.pts().or_else(|| packet.dts())
                && let Some(duration) = info.duration
            {
                let seconds = timestamp as f64 * video_input_time_base.numerator() as f64
                    / video_input_time_base.denominator() as f64;
                let percent = (seconds / duration * 100.0).clamp(0.0, 100.0);
                if percent - last_progress >= 1.0 {
                    progress(percent);
                    last_progress = percent;
                }
            }
            if let Some(video) = &mut video {
                let time_base = output_time_bases[video.output_index];
                video.process_packet(&packet, output, time_base)?;
            } else if let Some(video) = &mut copied_video {
                let time_base = output_time_bases[video.output_index];
                video.write_packet(packet, output, time_base, "H.264 video")?;
            }
        } else if let Some(audio) = &mut audio
            && stream.index() == audio.input_index
        {
            let mut packet = packet;
            packet.rescale_ts(stream.time_base(), audio.decoder_time_base);
            let time_base = output_time_bases[audio.output_index];
            audio.process_packet(&packet, output, time_base)?;
        } else if let Some(audio) = &mut copied_audio
            && stream.index() == audio.input_index
        {
            let time_base = output_time_bases[audio.output_index];
            audio.write_packet(packet, output, time_base, "AAC audio")?;
        }
    }

    if let Some(video) = &mut video {
        let video_time_base = output_time_bases[video.output_index];
        video.finish(output, video_time_base)?;
    }
    if let Some(video) = &mut copied_video {
        video.finish_pending(output, "H.264 video")?;
    }
    if let Some(audio) = &mut audio {
        let audio_time_base = output_time_bases[audio.output_index];
        audio.finish(output, audio_time_base)?;
    }
    if let Some(audio) = &mut copied_audio {
        audio.finish_pending(output, "AAC audio")?;
    }
    output
        .write_trailer()
        .context("could not finish the compatibility media output")?;
    progress(100.0);
    Ok(())
}

pub fn compatible_dimensions(width: u32, height: u32) -> (u32, u32) {
    let scale = f64::min(
        1.0,
        f64::min(
            MAX_OUTPUT_WIDTH as f64 / width.max(1) as f64,
            MAX_OUTPUT_HEIGHT as f64 / height.max(1) as f64,
        ),
    );
    let even = |value: u32| value.max(2) & !1;
    (
        even((width as f64 * scale).round() as u32),
        even((height as f64 * scale).round() as u32),
    )
}

pub fn temporary_mp4_path() -> Result<(tempfile::TempDir, PathBuf)> {
    let directory = tempfile::Builder::new()
        .prefix("cast-transcode-")
        .tempdir()
        .context("could not create a temporary media directory")?;
    let path = directory.path().join("prepared.mp4");
    Ok((directory, path))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn media(container: &str, video: codec::Id, audio: Option<codec::Id>) -> MediaInfo {
        MediaInfo {
            container: container.to_owned(),
            duration: Some(10.0),
            video: VideoInfo {
                stream_index: 0,
                codec: video,
                codec_name: video.name().to_owned(),
                width: 1920,
                height: 1080,
                frame_rate: Some(30.0),
                pixel_format: Pixel::YUV420P,
            },
            audio: audio.map(|codec| AudioInfo {
                stream_index: 1,
                codec,
                codec_name: codec.name().to_owned(),
                sample_rate: 48_000,
                channels: 2,
            }),
        }
    }

    #[test]
    fn identifies_container_families_from_ffmpeg_names() {
        assert_eq!(
            container_family("mov,mp4,m4a,3gp,3g2,mj2"),
            ContainerFamily::Mp4
        );
        assert_eq!(container_family("matroska,webm"), ContainerFamily::WebM);
        assert_eq!(container_family("avi"), ContainerFamily::Other);
    }

    #[test]
    fn directly_serves_conservative_mp4_and_webm_combinations() {
        assert_eq!(
            plan(
                &media("mov,mp4", codec::Id::H264, Some(codec::Id::AAC)),
                CompatibilityMode::Auto
            )
            .unwrap(),
            PreparationPlan::Direct {
                content_type: "video/mp4".to_owned()
            }
        );
        assert_eq!(
            plan(
                &media("matroska,webm", codec::Id::VP9, Some(codec::Id::OPUS)),
                CompatibilityMode::Auto
            )
            .unwrap(),
            PreparationPlan::Direct {
                content_type: "video/webm".to_owned()
            }
        );
    }

    #[test]
    fn remuxes_compatible_streams_from_an_incompatible_container() {
        assert!(matches!(
            plan(
                &media("matroska", codec::Id::H264, Some(codec::Id::AAC)),
                CompatibilityMode::Auto
            )
            .unwrap(),
            PreparationPlan::Remux { .. }
        ));
    }

    #[test]
    fn transcodes_incompatible_video_or_audio() {
        let PreparationPlan::Transcode { tracks, .. } = plan(
            &media("matroska", codec::Id::HEVC, Some(codec::Id::OPUS)),
            CompatibilityMode::Auto,
        )
        .unwrap()
        else {
            panic!("incompatible media was not selected for transcoding");
        };
        assert_eq!(tracks, TranscodeTracks::all(true));
    }

    #[test]
    fn transcodes_only_the_incompatible_track() {
        let PreparationPlan::Transcode {
            tracks: audio_only,
            ..
        } = plan(
            &media("matroska", codec::Id::H264, Some(codec::Id::EAC3)),
            CompatibilityMode::Auto,
        )
        .unwrap()
        else {
            panic!("incompatible audio was not selected for transcoding");
        };
        assert_eq!(
            audio_only,
            TranscodeTracks {
                video: false,
                audio: true,
            }
        );

        let PreparationPlan::Transcode {
            tracks: video_only,
            ..
        } = plan(
            &media("matroska", codec::Id::HEVC, Some(codec::Id::AAC)),
            CompatibilityMode::Auto,
        )
        .unwrap()
        else {
            panic!("incompatible video was not selected for transcoding");
        };
        assert_eq!(
            video_only,
            TranscodeTracks {
                video: true,
                audio: false,
            }
        );
    }

    #[test]
    fn repairs_small_backward_encoder_timestamp_jumps() {
        let mut last_dts = None;
        assert_eq!(
            monotonic_packet_timestamps(Some(100), Some(100), &mut last_dts),
            (Some(100), Some(100))
        );
        assert_eq!(last_dts, Some(100));
        assert_eq!(
            monotonic_packet_timestamps(Some(84), Some(84), &mut last_dts),
            (Some(101), Some(101))
        );
        assert_eq!(last_dts, Some(101));
        assert_eq!(
            monotonic_packet_timestamps(Some(220), Some(225), &mut last_dts),
            (Some(220), Some(225))
        );
        assert_eq!(last_dts, Some(220));
    }

    #[test]
    fn never_mode_rejects_non_direct_media() {
        assert!(
            plan(
                &media("matroska", codec::Id::H264, Some(codec::Id::AAC)),
                CompatibilityMode::Never
            )
            .is_err()
        );
    }

    #[test]
    fn normalizes_oversized_otherwise_compatible_video() {
        let mut input = media("mov,mp4", codec::Id::H264, Some(codec::Id::AAC));
        input.video.width = 3840;
        input.video.height = 2160;
        assert!(matches!(
            plan(&input, CompatibilityMode::Auto).unwrap(),
            PreparationPlan::Transcode { .. }
        ));
    }

    #[test]
    fn compatibility_dimensions_preserve_size_or_fit_inside_1080p() {
        assert_eq!(compatible_dimensions(1280, 720), (1280, 720));
        assert_eq!(compatible_dimensions(3840, 2160), (1920, 1080));
        assert_eq!(compatible_dimensions(1080, 1920), (608, 1080));
    }

    #[test]
    fn inspection_rejects_non_media_input() {
        let mut input = tempfile::NamedTempFile::new().unwrap();
        input.write_all(b"not a media file").unwrap();
        input.flush().unwrap();
        assert!(inspect(input.path()).is_err());
    }
}
