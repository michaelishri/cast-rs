#[cfg(target_os = "macos")]
mod audio;
#[cfg(target_os = "macos")]
mod capture;
mod cast;
mod desktop;
mod discovery;
#[cfg(target_os = "macos")]
mod live;
mod media;
mod media_server;
#[cfg(target_os = "macos")]
mod mirror;
mod network;
mod playback;
mod player_controls;
#[cfg(target_os = "linux")]
mod setup;
#[cfg(target_os = "macos")]
mod synthetic;
mod tui;
mod video;
#[cfg(target_os = "macos")]
mod virtual_display;
mod vod_hls;

use std::{
    fmt::Write,
    io::{self, IsTerminal},
    net::IpAddr,
    path::PathBuf,
    thread,
    time::Duration,
};

use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    version,
    about = "Cast local video and a desktop to Google Cast devices"
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
    #[cfg(target_os = "linux")]
    /// Install or check the optional Cisco OpenH264 runtime module.
    Setup {
        /// Confirm the verified download without prompting.
        #[arg(long)]
        yes: bool,
        /// Check local availability without downloading or changing files.
        #[arg(long, conflicts_with = "yes")]
        check: bool,
    },
    #[cfg(target_os = "macos")]
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
        /// Local HTTP port; 0 asks the OS to select an available port.
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
        #[cfg(target_os = "linux")]
        /// Linux H.264 encoder used when video conversion is required.
        #[arg(long, value_enum, default_value_t = H264Encoder::Auto)]
        encoder: H264Encoder,
        /// Deliver transcoded media as it is prepared or after a complete MP4 is ready.
        #[arg(long, value_enum, default_value_t = TranscodeDeliveryMode::Incremental)]
        transcode_delivery: TranscodeDeliveryMode,
    },
    /// Browse local videos, manage a queue, and control playback in a full-screen interface.
    Tui {
        /// Directory to browse; defaults to the current directory.
        #[arg(default_value = ".")]
        directory: PathBuf,
        /// Chromecast IP address; when omitted the TUI discovers receivers.
        #[arg(long)]
        host: Option<IpAddr>,
        /// Cast control port.
        #[arg(long, visible_alias = "port", default_value_t = 8009)]
        cast_port: u16,
        /// Local HTTP port; 0 asks the OS to select an available port.
        #[arg(long, default_value_t = 0)]
        http_port: u16,
        /// Convert incompatible containers/codecs using linked media libraries.
        #[arg(long, value_enum, default_value_t = TranscodeMode::Auto)]
        transcode: TranscodeMode,
        #[cfg(target_os = "linux")]
        /// Linux H.264 encoder used when video conversion is required.
        #[arg(long, value_enum, default_value_t = H264Encoder::Auto)]
        encoder: H264Encoder,
        /// Deliver transcoded media incrementally or after a complete MP4 is ready.
        #[arg(long, value_enum, default_value_t = TranscodeDeliveryMode::Incremental)]
        transcode_delivery: TranscodeDeliveryMode,
    },
    #[cfg(target_os = "macos")]
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
    #[cfg(target_os = "macos")]
    /// Measure the mirroring path and recommend receiver latency settings.
    Profile {
        /// Chromecast IP address; repeat --host to profile a receiver group.
        #[arg(long, required = true, action = ArgAction::Append)]
        host: Vec<IpAddr>,
        #[arg(long, default_value_t = 8009)]
        cast_port: u16,
        /// CoreGraphics display ID; defaults to the first display.
        #[arg(long)]
        display: Option<u32>,
        /// Create one temporary extended display per receiver using an experimental private macOS API.
        #[arg(
            long,
            conflicts_with_all = ["display", "synthetic", "auto_tune"]
        )]
        extend: bool,
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
    #[cfg(target_os = "macos")]
    /// Capture this Mac's desktop and cast it to one or more Google Cast receivers.
    Desktop {
        /// Chromecast IP address; repeat --host to cast to multiple receivers.
        #[arg(long, required = true, action = ArgAction::Append)]
        host: Vec<IpAddr>,
        #[arg(long, default_value_t = 8009)]
        cast_port: u16,
        /// Transport to use; mirror targets sub-second latency, HLS is the compatibility fallback.
        #[arg(long, value_enum, default_value_t = DesktopTransport::Mirror)]
        transport: DesktopTransport,
        /// CoreGraphics display ID; defaults to the first display.
        #[arg(long)]
        display: Option<u32>,
        /// Create one temporary extended display per receiver using an experimental private macOS API.
        #[arg(long, conflicts_with = "display")]
        extend: bool,
        /// Include system and application audio (microphone audio is never captured).
        #[arg(long)]
        audio: bool,
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
        /// Start capture and HLS serving without contacting the sole receiver (HLS only).
        #[arg(long)]
        serve_only: bool,
    },
    #[cfg(target_os = "macos")]
    /// Internal owner process for a temporary virtual display.
    #[command(name = "__virtual-display-helper", hide = true)]
    VirtualDisplayHelper {
        #[arg(long, value_parser = clap::value_parser!(u32).range(2..=3840))]
        width: u32,
        #[arg(long, value_parser = clap::value_parser!(u32).range(2..=2160))]
        height: u32,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=60))]
        fps: u32,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        serial: u32,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        ordinal: u32,
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

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, ValueEnum)]
enum H264Encoder {
    Auto,
    Nvenc,
    Vaapi,
    Openh264,
}

#[cfg(target_os = "linux")]
impl From<H264Encoder> for media::H264Provider {
    fn from(value: H264Encoder) -> Self {
        match value {
            H264Encoder::Auto => Self::Auto,
            H264Encoder::Nvenc => Self::Nvenc,
            H264Encoder::Vaapi => Self::Vaapi,
            H264Encoder::Openh264 => Self::Openh264,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum TranscodeDeliveryMode {
    Complete,
    Incremental,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let interactive_video = interactive_video_output(
        cli.verbose,
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
    );
    let log_capture = matches!(&cli.command, Command::Tui { .. }).then(tui::LogCapture::new);
    init_logging(
        cli.verbose,
        log_capture.as_ref().map(tui::LogCapture::writer),
    );
    match cli.command {
        Command::Devices { timeout } => {
            let devices = discovery::discover(Duration::from_secs(timeout))?;
            if devices.is_empty() {
                println!("No Cast devices found.");
            } else {
                print_devices(&devices);
            }
        }
        #[cfg(target_os = "linux")]
        Command::Setup { yes, check } => setup::run(yes, check)?,
        #[cfg(target_os = "macos")]
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
            #[cfg(target_os = "linux")]
            encoder,
            transcode_delivery,
        } => {
            #[cfg(target_os = "linux")]
            media::configure_h264_provider(encoder.into());
            video::cast_video(video::VideoOptions {
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
                transcode_delivery: match transcode_delivery {
                    TranscodeDeliveryMode::Complete => video::TranscodeDelivery::Complete,
                    TranscodeDeliveryMode::Incremental => video::TranscodeDelivery::Incremental,
                },
                interactive: interactive_video,
            })?
        }
        Command::Tui {
            directory,
            host,
            cast_port,
            http_port,
            transcode,
            #[cfg(target_os = "linux")]
            encoder,
            transcode_delivery,
        } => {
            #[cfg(target_os = "linux")]
            media::configure_h264_provider(encoder.into());
            tui::run(
                tui::TuiOptions {
                    directory,
                    host,
                    cast_port,
                    http_port,
                    compatibility_mode: match transcode {
                        TranscodeMode::Auto => media::CompatibilityMode::Auto,
                        TranscodeMode::Never => media::CompatibilityMode::Never,
                        TranscodeMode::Always => media::CompatibilityMode::Always,
                    },
                    transcode_delivery: match transcode_delivery {
                        TranscodeDeliveryMode::Complete => video::TranscodeDelivery::Complete,
                        TranscodeDeliveryMode::Incremental => video::TranscodeDelivery::Incremental,
                    },
                },
                log_capture
                    .expect("TUI log capture was initialized")
                    .receiver,
            )?
        }
        #[cfg(target_os = "macos")]
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
        #[cfg(target_os = "macos")]
        Command::Profile {
            host,
            cast_port,
            display,
            extend,
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
                cast_hosts: host,
                cast_port,
                display_id: display,
                extend,
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
                audio: false,
            },
            auto_tune,
        )?,
        #[cfg(target_os = "macos")]
        Command::Desktop {
            host,
            cast_port,
            transport,
            display,
            extend,
            audio,
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
                    cast_hosts: host,
                    cast_port,
                    display_id: display,
                    extend,
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
                    audio,
                })?;
            }
            DesktopTransport::Hls => {
                if max_frame_age_ms.is_some() || fixed_bitrate || quality_priority {
                    anyhow::bail!(
                        "--max-frame-age-ms, --fixed-bitrate, and --quality-priority apply only to --transport mirror"
                    );
                }
                live::cast_desktop(live::LiveOptions {
                    cast_hosts: host,
                    cast_port,
                    display_id: display,
                    extend,
                    http_port,
                    fps,
                    width,
                    height,
                    bitrate,
                    duration: seconds.map(Duration::from_secs),
                    serve_only,
                    audio,
                })?;
            }
        },
        #[cfg(target_os = "macos")]
        Command::VirtualDisplayHelper {
            width,
            height,
            fps,
            serial,
            ordinal,
        } => {
            virtual_display::run_helper(width, height, fps, serial, ordinal)?;
        }
    }

    Ok(())
}

fn print_devices(devices: &[discovery::CastService]) {
    print!("{}", format_devices(devices));
}

fn format_devices(devices: &[discovery::CastService]) -> String {
    let rows: Vec<_> = devices
        .iter()
        .map(|device| {
            [
                device.name.clone(),
                device.address.to_string(),
                device.port.to_string(),
                device.model.clone(),
                device.capability.label().to_owned(),
            ]
        })
        .collect();
    let headers = ["NAME", "ADDRESS", "PORT", "MODEL", "CAPABILITY"];
    let widths = headers.iter().enumerate().map(|(index, header)| {
        rows.iter()
            .map(|row| row[index].chars().count())
            .chain(std::iter::once(header.chars().count()))
            .max()
            .expect("headers are not empty")
    });
    let widths: Vec<_> = widths.collect();

    let mut output = String::new();
    write_row(&mut output, &headers, &widths);
    for row in &rows {
        write_row(&mut output, row, &widths);
    }
    output
}

fn write_row<T: AsRef<str>>(output: &mut String, row: &[T], widths: &[usize]) {
    for (index, (value, width)) in row.iter().zip(widths).enumerate() {
        if index > 0 {
            output.push_str("  ");
        }
        write!(output, "{:<width$}", value.as_ref(), width = width)
            .expect("writing to a String cannot fail");
    }
    output.push('\n');
}

#[cfg(test)]
mod device_tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::{discovery, format_devices};

    #[test]
    fn device_table_aligns_columns_and_shows_capabilities() {
        let devices = [
            discovery::CastService {
                name: "Clock".into(),
                address: IpAddr::V4(Ipv4Addr::new(192, 168, 86, 192)),
                port: 8009,
                model: "Lenovo Smart Clock".into(),
                capability: discovery::DeviceCapability::AudioOnly,
            },
            discovery::CastService {
                name: "Living Room TV".into(),
                address: IpAddr::V4(Ipv4Addr::new(192, 168, 86, 73)),
                port: 8009,
                model: "Google TV Streamer".into(),
                capability: discovery::DeviceCapability::Video,
            },
        ];

        assert_eq!(
            format_devices(&devices),
            concat!(
                "NAME            ADDRESS         PORT  MODEL               CAPABILITY   \n",
                "Clock           192.168.86.192  8009  Lenovo Smart Clock  Audio only   \n",
                "Living Room TV  192.168.86.73   8009  Google TV Streamer  Audio + video\n",
            )
        );
    }
}

fn init_logging(verbosity: u8, target: Option<tui::LogWriter>) {
    let level = match verbosity {
        0 => log::LevelFilter::Warn,
        1 => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    };
    let mut builder = env_logger::Builder::new();
    builder
        .filter_level(log::LevelFilter::Warn)
        .filter_module("cast", level)
        .filter_module("rust_cast", level)
        .format_timestamp_millis();
    if let Some(target) = target {
        builder.target(env_logger::Target::Pipe(Box::new(target)));
    }
    builder.init();
}

fn interactive_video_output(verbosity: u8, stdin_terminal: bool, stdout_terminal: bool) -> bool {
    verbosity == 0 && stdin_terminal && stdout_terminal
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::H264Encoder;
    use super::{Cli, Command, TranscodeDeliveryMode, TranscodeMode, interactive_video_output};
    #[cfg(not(target_os = "macos"))]
    use clap::CommandFactory;
    use clap::{Parser, error::ErrorKind};
    #[cfg(target_os = "macos")]
    use std::net::IpAddr;

    #[test]
    fn interactive_video_requires_default_verbosity_and_terminal_io() {
        assert!(interactive_video_output(0, true, true));
        assert!(!interactive_video_output(1, true, true));
        assert!(!interactive_video_output(2, true, true));
        assert!(!interactive_video_output(0, false, true));
        assert!(!interactive_video_output(0, true, false));
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn linux_help_is_portable_and_desktop_commands_are_hidden() {
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("macOS"));

        for command in ["displays", "capture", "profile", "desktop"] {
            let error = Cli::try_parse_from(["cast", command])
                .err()
                .expect("an incomplete desktop command was exposed on Linux");
            assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_setup_and_encoder_selection_parse_without_exposing_desktop_early() {
        let cli = Cli::try_parse_from(["cast", "setup", "--check"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Setup {
                check: true,
                yes: false
            }
        ));
        let error = Cli::try_parse_from(["cast", "setup", "--check", "--yes"])
            .err()
            .expect("conflicting setup switches were accepted");
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);

        let cli = Cli::try_parse_from([
            "cast",
            "video",
            "sample.mp4",
            "--host",
            "192.0.2.1",
            "--encoder",
            "vaapi",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Video {
                encoder: H264Encoder::Vaapi,
                ..
            }
        ));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn desktop_accepts_extend_as_a_switch() {
        let cli =
            Cli::try_parse_from(["cast", "desktop", "--host", "192.0.2.1", "--extend"]).unwrap();
        assert!(matches!(cli.command, Command::Desktop { extend: true, .. }));
    }

    #[test]
    fn tui_parses_defaults_and_every_option() {
        let cli = Cli::try_parse_from(["cast", "tui"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Tui {
                ref directory,
                host: None,
                cast_port: 8009,
                http_port: 0,
                transcode: TranscodeMode::Auto,
                transcode_delivery: TranscodeDeliveryMode::Incremental,
                ..
            } if directory == std::path::Path::new(".")
        ));

        let cli = Cli::try_parse_from([
            "cast",
            "tui",
            "/tmp",
            "--host",
            "192.0.2.1",
            "--cast-port",
            "9000",
            "--http-port",
            "8080",
            "--transcode",
            "always",
            "--transcode-delivery",
            "complete",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Tui {
                ref directory,
                host: Some(_),
                cast_port: 9000,
                http_port: 8080,
                transcode: TranscodeMode::Always,
                transcode_delivery: TranscodeDeliveryMode::Complete,
                ..
            } if directory == std::path::Path::new("/tmp")
        ));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn desktop_audio_is_opt_in() {
        let cli =
            Cli::try_parse_from(["cast", "desktop", "--host", "192.0.2.1", "--audio"]).unwrap();
        assert!(matches!(cli.command, Command::Desktop { audio: true, .. }));

        let cli = Cli::try_parse_from(["cast", "desktop", "--host", "192.0.2.1"]).unwrap();
        assert!(matches!(cli.command, Command::Desktop { audio: false, .. }));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn extend_conflicts_with_an_explicit_display() {
        let result = Cli::try_parse_from([
            "cast",
            "desktop",
            "--host",
            "192.0.2.1",
            "--extend",
            "--display",
            "42",
        ]);
        let error = result.err().expect("conflicting arguments were accepted");
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn profile_extend_conflicts_with_synthetic_input() {
        let result = Cli::try_parse_from([
            "cast",
            "profile",
            "--host",
            "192.0.2.1",
            "--extend",
            "--synthetic",
        ]);
        let error = result.err().expect("conflicting arguments were accepted");
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn desktop_preserves_repeated_host_order() {
        let cli = Cli::try_parse_from([
            "cast",
            "desktop",
            "--host",
            "192.0.2.10",
            "--host",
            "192.0.2.20",
        ])
        .unwrap();
        let Command::Desktop { host, .. } = cli.command else {
            panic!("desktop command was not parsed");
        };
        assert_eq!(
            host,
            [
                "192.0.2.10".parse::<IpAddr>().unwrap(),
                "192.0.2.20".parse::<IpAddr>().unwrap(),
            ]
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn profile_accepts_a_receiver_group() {
        let cli = Cli::try_parse_from([
            "cast",
            "profile",
            "--host",
            "2001:db8::10",
            "--host",
            "2001:db8::20",
        ])
        .unwrap();
        let Command::Profile { host, .. } = cli.command else {
            panic!("profile command was not parsed");
        };
        assert_eq!(host.len(), 2);
        assert_eq!(host[0], "2001:db8::10".parse::<IpAddr>().unwrap());
        assert_eq!(host[1], "2001:db8::20".parse::<IpAddr>().unwrap());
    }
}
