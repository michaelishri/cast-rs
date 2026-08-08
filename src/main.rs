mod capture;
mod cast;
mod discovery;
mod live;
mod media;
mod media_server;
mod mirror;
mod network;
mod synthetic;
mod video;

use std::{net::IpAddr, path::PathBuf, thread, time::Duration};

use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    version,
    about = "Cast local video and a macOS desktop to Google Cast devices"
)]
struct Cli {
    /// Increase diagnostic output; use -vv for frame-level tracing.
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Discover Google Cast devices on the local network.
    Devices {
        #[arg(long, default_value_t = 3)]
        timeout: u64,
    },
    /// List displays visible to macOS ScreenCaptureKit.
    Displays,
    /// Ask a Cast device to play an existing media URL.
    Url {
        /// Chromecast IP address (shown by `cast devices`).
        #[arg(long)]
        host: IpAddr,
        #[arg(long, default_value_t = 8009)]
        port: u16,
        #[arg(long)]
        url: String,
        /// MIME type advertised to the Cast receiver.
        #[arg(long, default_value = "application/x-mpegURL")]
        content_type: String,
        /// Packaging used by HLS video segments; required for fMP4/CMAF playlists.
        #[arg(long, value_enum)]
        hls_video_segment_format: Option<HlsVideoFormat>,
        #[arg(long, value_enum, default_value_t = StreamKind::Live)]
        stream: StreamKind,
        /// Keep the sender connected for this many seconds to log asynchronous receiver status.
        #[arg(long, default_value_t = 0)]
        monitor_seconds: u64,
    },
    /// Play a local video, converting it when needed for Cast compatibility.
    Video {
        /// Local video file to play.
        file: PathBuf,
        /// Chromecast IP address (shown by `cast devices`).
        #[arg(long)]
        host: IpAddr,
        /// Cast control port.
        #[arg(long, visible_alias = "port", default_value_t = 8009)]
        cast_port: u16,
        /// Local HTTP port; 0 asks macOS to select an available port.
        #[arg(long, default_value_t = 0)]
        http_port: u16,
        /// Initial playback position in seconds.
        #[arg(long, default_value_t = 0.0)]
        start_at: f64,
        /// Expert MIME override; bypasses auto preparation unless transcoding is forced.
        #[arg(long)]
        content_type: Option<String>,
        /// Convert incompatible containers/codecs using linked media libraries.
        #[arg(long, value_enum, default_value_t = TranscodeMode::Auto)]
        transcode: TranscodeMode,
    },
    /// Capture and hardware-encode a short H.264/AVCC diagnostic sample.
    Capture {
        /// CoreGraphics display ID; defaults to the first display.
        #[arg(long)]
        display: Option<u32>,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=60))]
        fps: u32,
        #[arg(long, default_value_t = 6_000_000)]
        bitrate: i32,
        /// Output is length-prefixed H.264 (AVCC), not yet a standalone media file.
        #[arg(long, default_value = "capture.avcc")]
        output: PathBuf,
    },
    /// Measure the mirroring path and recommend receiver latency settings.
    Profile {
        /// Chromecast IP address (shown by `cast devices`).
        #[arg(long)]
        host: IpAddr,
        #[arg(long, default_value_t = 8009)]
        cast_port: u16,
        /// CoreGraphics display ID; defaults to the first display.
        #[arg(long)]
        display: Option<u32>,
        /// Profiling duration; with --auto-tune this is divided across all trials.
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(10..=3600))]
        seconds: u64,
        /// Small receiver buffer used while measuring the unconcealed latency tail.
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=5000))]
        probe_delay_ms: u64,
        /// Use a deterministic synthetic workload instead of capturing the desktop.
        #[arg(long)]
        synthetic: bool,
        /// Compare latency controls in six synthetic trials and recommend the measured winner.
        #[arg(long, requires = "synthetic")]
        auto_tune: bool,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=60))]
        fps: u32,
        /// Encoded output width used for both the profile and its recommendation.
        #[arg(long, default_value_t = 1280, value_parser = clap::value_parser!(u32).range(2..=3840))]
        width: u32,
        /// Encoded output height used for both the profile and its recommendation.
        #[arg(long, default_value_t = 720, value_parser = clap::value_parser!(u32).range(2..=2160))]
        height: u32,
        /// VideoToolbox target bitrate used for the profile.
        #[arg(long, default_value_t = 6_000_000)]
        bitrate: i32,
        /// Drop a raw frame if it waits this long before encoding; defaults to two frame periods. Use 0 to disable.
        #[arg(long, value_parser = clap::value_parser!(u64).range(0..=1000))]
        max_frame_age_ms: Option<u64>,
        /// Keep the requested bitrate fixed instead of adapting it to receiver feedback.
        #[arg(long)]
        fixed_bitrate: bool,
        /// Prefer encoding quality over VideoToolbox's lowest-latency speed path.
        #[arg(long)]
        quality_priority: bool,
    },
    /// Capture this Mac's desktop and cast it to a Google Cast receiver.
    Desktop {
        /// Chromecast IP address (shown by `cast devices`).
        #[arg(long)]
        host: IpAddr,
        #[arg(long, default_value_t = 8009)]
        cast_port: u16,
        /// Transport to use; mirror targets sub-second latency, HLS is the compatibility fallback.
        #[arg(long, value_enum, default_value_t = DesktopTransport::Mirror)]
        transport: DesktopTransport,
        /// CoreGraphics display ID; defaults to the first display.
        #[arg(long)]
        display: Option<u32>,
        /// Receiver playout buffer for the mirroring transport, in milliseconds.
        #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u64).range(1..=5000))]
        target_delay_ms: u64,
        /// HTTP port used only by the HLS transport.
        #[arg(long, default_value_t = 8080)]
        http_port: u16,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=60))]
        fps: u32,
        /// Encoded output width; defaults to the Nest Hub-compatible 720p width.
        #[arg(long, default_value_t = 1280, value_parser = clap::value_parser!(u32).range(2..=3840))]
        width: u32,
        /// Encoded output height; defaults to the Nest Hub-compatible 720p height.
        #[arg(long, default_value_t = 720, value_parser = clap::value_parser!(u32).range(2..=2160))]
        height: u32,
        #[arg(long, default_value_t = 6_000_000)]
        bitrate: i32,
        /// Drop a raw frame if it waits this long before encoding; defaults to two frame periods. Use 0 to disable.
        #[arg(long, value_parser = clap::value_parser!(u64).range(0..=1000))]
        max_frame_age_ms: Option<u64>,
        /// Keep the requested bitrate fixed instead of adapting it to receiver feedback.
        #[arg(long)]
        fixed_bitrate: bool,
        /// Prefer encoding quality over VideoToolbox's lowest-latency speed path.
        #[arg(long)]
        quality_priority: bool,
        /// Stop automatically after this many seconds; otherwise run until Ctrl-C.
        #[arg(long)]
        seconds: Option<u64>,
        /// Start capture and HLS serving without sending a command to the receiver (HLS only).
        #[arg(long)]
        serve_only: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum StreamKind {
    Live,
    Buffered,
}

#[derive(Clone, Copy, ValueEnum)]
enum HlsVideoFormat {
    Fmp4,
    Mpeg2Ts,
}

#[derive(Clone, Copy, ValueEnum)]
enum DesktopTransport {
    Mirror,
    Hls,
}

#[derive(Clone, Copy, ValueEnum)]
enum TranscodeMode {
    Auto,
    Never,
    Always,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    match cli.command {
        Command::Devices { timeout } => {
            let devices = discovery::discover(Duration::from_secs(timeout))?;
            if devices.is_empty() {
                println!("No Cast devices found.");
            } else {
                println!("NAME\tADDRESS\tPORT\tMODEL");
                for device in devices {
                    println!(
                        "{}\t{}\t{}\t{}",
                        device.name, device.address, device.port, device.model
                    );
                }
            }
        }
        Command::Displays => capture::list_displays()?,
        Command::Url {
            host,
            port,
            url,
            content_type,
            hls_video_segment_format,
            stream,
            monitor_seconds,
        } => {
            let live = matches!(stream, StreamKind::Live);
            if let Some(format) = hls_video_segment_format {
                let format = match format {
                    HlsVideoFormat::Fmp4 => cast::HlsVideoSegmentFormat::Fmp4,
                    HlsVideoFormat::Mpeg2Ts => cast::HlsVideoSegmentFormat::Mpeg2Ts,
                };
                cast::cast_url_with_hls_video_format(
                    host,
                    port,
                    &url,
                    &content_type,
                    live,
                    format,
                )?;
            } else {
                cast::cast_url(host, port, &url, &content_type, live)?;
            }
            if monitor_seconds > 0 {
                thread::sleep(Duration::from_secs(monitor_seconds));
            }
        }
        Command::Video {
            file,
            host,
            cast_port,
            http_port,
            start_at,
            content_type,
            transcode,
        } => video::cast_video(video::VideoOptions {
            cast_host: host,
            cast_port,
            http_port,
            file,
            start_at,
            content_type,
            compatibility_mode: match transcode {
                TranscodeMode::Auto => media::CompatibilityMode::Auto,
                TranscodeMode::Never => media::CompatibilityMode::Never,
                TranscodeMode::Always => media::CompatibilityMode::Always,
            },
        })?,
        Command::Capture {
            display,
            seconds,
            fps,
            bitrate,
            output,
        } => capture::capture(capture::CaptureOptions {
            display_id: display,
            duration: Duration::from_secs(seconds),
            fps,
            bitrate,
            output,
        })?,
        Command::Profile {
            host,
            cast_port,
            display,
            seconds,
            probe_delay_ms,
            synthetic,
            auto_tune,
            fps,
            width,
            height,
            bitrate,
            max_frame_age_ms,
            fixed_bitrate,
            quality_priority,
        } => mirror::profile_desktop(
            mirror::MirrorOptions {
                cast_host: host,
                cast_port,
                display_id: display,
                fps,
                width,
                height,
                bitrate,
                target_delay: Duration::from_millis(probe_delay_ms),
                duration: Some(Duration::from_secs(seconds)),
                synthetic,
                max_frame_age: max_frame_age_ms.map(Duration::from_millis),
                adaptive_bitrate: !fixed_bitrate,
                prioritize_encoding_speed: !quality_priority,
            },
            auto_tune,
        )?,
        Command::Desktop {
            host,
            cast_port,
            transport,
            display,
            target_delay_ms,
            http_port,
            fps,
            width,
            height,
            bitrate,
            max_frame_age_ms,
            fixed_bitrate,
            quality_priority,
            seconds,
            serve_only,
        } => match transport {
            DesktopTransport::Mirror => {
                if serve_only {
                    anyhow::bail!("--serve-only is available only with --transport hls");
                }
                mirror::cast_desktop(mirror::MirrorOptions {
                    cast_host: host,
                    cast_port,
                    display_id: display,
                    fps,
                    width,
                    height,
                    bitrate,
                    target_delay: Duration::from_millis(target_delay_ms),
                    duration: seconds.map(Duration::from_secs),
                    synthetic: false,
                    max_frame_age: max_frame_age_ms.map(Duration::from_millis),
                    adaptive_bitrate: !fixed_bitrate,
                    prioritize_encoding_speed: !quality_priority,
                })?;
            }
            DesktopTransport::Hls => {
                if max_frame_age_ms.is_some() || fixed_bitrate || quality_priority {
                    anyhow::bail!(
                        "--max-frame-age-ms, --fixed-bitrate, and --quality-priority apply only to --transport mirror"
                    );
                }
                live::cast_desktop(live::LiveOptions {
                    cast_host: host,
                    cast_port,
                    display_id: display,
                    http_port,
                    fps,
                    width,
                    height,
                    bitrate,
                    duration: seconds.map(Duration::from_secs),
                    serve_only,
                })?;
            }
        },
    }

    Ok(())
}

fn init_logging(verbosity: u8) {
    let level = match verbosity {
        0 => log::LevelFilter::Warn,
        1 => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    };
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Warn)
        .filter_module("cast", level)
        .filter_module("rust_cast", level)
        .format_timestamp_millis()
        .init();
}
