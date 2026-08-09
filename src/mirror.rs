use std::{
    collections::{HashSet, VecDeque},
    ffi::c_void,
    io::{self, ErrorKind, IsTerminal, Write},
    net::{IpAddr, SocketAddr, UdpSocket},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use aes::{
    Aes128,
    cipher::{KeyIvInit, StreamCipher},
};
use anyhow::{Context, Result, anyhow, bail};
use ctr::Ctr128BE;
use rust_cast::{
    CastDevice, ChannelMessage,
    channels::{
        connection::ConnectionResponse, heartbeat::HeartbeatResponse, receiver::CastDeviceApp,
    },
    errors::Error as CastError,
    message_manager::{CastMessage, CastMessagePayload},
};
use screencapturekit::IOSurface;
use screencapturekit::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use videotoolbox::ProfileLevel;
use videotoolbox::prelude::*;

use crate::audio::{
    self, AudioFrameHandler, AudioWorker, LocalOutputControl, LocalOutputRedirect, MediaClock,
};
use crate::synthetic::{
    SYNTHETIC_CYCLE_SECONDS, SYNTHETIC_WORKLOAD_NAME, SyntheticFrameGenerator, SyntheticPhase,
    phase_for_frame,
};
use crate::virtual_display::VirtualDisplaySession;

const CAST_STREAMING_APP_ID: &str = "0F5096E8";
const CAST_STREAMING_NAMESPACE: &str = "urn:x-cast:com.google.cast.webrtc";
const RTP_VIDEO_TIMEBASE: u64 = 90_000;
const RTP_H264_PAYLOAD_TYPE: u8 = 101;
const RTCP_REPORT_INTERVAL: Duration = Duration::from_millis(500);
const MAX_UNACKED_FRAMES: usize = 120;
const DELTA_PACKETS_PER_BURST: usize = 16;
const KEYFRAME_PACKETS_PER_BURST: usize = 24;
const MAX_FRAME_PACING_WINDOW: Duration = Duration::from_millis(5);
const RATE_CONTROL_INTERVAL: Duration = Duration::from_secs(1);
const RATE_CONTROL_HEALTHY_WINDOWS_BEFORE_INCREASE: u8 = 3;
static VIRTUAL_DISPLAY_TEARDOWN: Mutex<()> = Mutex::new(());
const SYNTHETIC_SURFACE_POOL_SIZE: usize = 3;
const PROFILE_HISTORY_SECONDS: usize = 60;
const PROFILE_GRAPH_COLUMNS: usize = 44;
const AUTO_TUNE_TRIAL_COUNT: usize = 6;
const AUTO_TUNE_MINIMUM_SECONDS: u64 = 60;
const AUTO_TUNE_NOISE_MARGIN_MS: f64 = 5.0;

type Aes128Ctr = Ctr128BE<Aes128>;

#[derive(Clone)]
pub struct MirrorOptions {
    pub cast_hosts: Vec<IpAddr>,
    pub cast_port: u16,
    pub display_id: Option<u32>,
    pub extend: bool,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    pub bitrate: i32,
    pub target_delay: Duration,
    pub duration: Option<Duration>,
    pub synthetic: bool,
    /// `None` selects two frame periods; zero explicitly disables raw-frame expiry.
    pub max_frame_age: Option<Duration>,
    pub adaptive_bitrate: bool,
    pub prioritize_encoding_speed: bool,
    pub audio: bool,
}

fn validate_cast_hosts(hosts: &[IpAddr]) -> Result<()> {
    if hosts.is_empty() {
        bail!("at least one --host is required");
    }
    let mut unique = HashSet::with_capacity(hosts.len());
    for host in hosts {
        if !unique.insert(*host) {
            bail!("duplicate Cast receiver host {host}");
        }
    }
    Ok(())
}

pub fn cast_desktop(options: MirrorOptions) -> Result<()> {
    validate_cast_hosts(&options.cast_hosts)?;
    let interrupted = install_interrupt_handler()?;
    if options.extend && options.cast_hosts.len() > 1 {
        run_extended_desktop(
            options,
            RunMode::Cast,
            ProfilePresentation::Detailed,
            &interrupted,
        )
        .map(|_| ())
    } else {
        run_desktop(
            options,
            RunMode::Cast,
            ProfilePresentation::Detailed,
            interrupted.as_ref(),
            None,
        )
        .map(|_| ())
    }
}

pub fn profile_desktop(options: MirrorOptions, auto_tune: bool) -> Result<()> {
    validate_cast_hosts(&options.cast_hosts)?;
    if options.duration.is_none() {
        bail!("latency profiling requires a fixed duration");
    }
    let interrupted = install_interrupt_handler()?;
    if auto_tune {
        auto_tune_profile(options, interrupted.as_ref())
    } else if options.extend && options.cast_hosts.len() > 1 {
        run_extended_desktop(
            options,
            RunMode::Profile,
            ProfilePresentation::Detailed,
            &interrupted,
        )
        .map(|_| ())
    } else {
        run_desktop(
            options,
            RunMode::Profile,
            ProfilePresentation::Detailed,
            interrupted.as_ref(),
            None,
        )
        .map(|_| ())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunMode {
    Cast,
    Profile,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProfilePresentation {
    Detailed,
    GroupedTarget,
    AutoTuneTrial,
}

fn install_interrupt_handler() -> Result<Arc<AtomicBool>> {
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&interrupted);
    ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))
        .context("could not install Ctrl-C handler")?;
    Ok(interrupted)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RawFrameDeadline {
    Automatic,
    OneFrame,
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TuningConfig {
    adaptive_bitrate: bool,
    prioritize_encoding_speed: bool,
    raw_frame_deadline: RawFrameDeadline,
}

impl TuningConfig {
    const BASELINE: Self = Self {
        adaptive_bitrate: true,
        prioritize_encoding_speed: true,
        raw_frame_deadline: RawFrameDeadline::Automatic,
    };

    fn apply(self, options: &mut MirrorOptions, effective_fps: u32) {
        options.adaptive_bitrate = self.adaptive_bitrate;
        options.prioritize_encoding_speed = self.prioritize_encoding_speed;
        options.max_frame_age = match self.raw_frame_deadline {
            RawFrameDeadline::Automatic => None,
            RawFrameDeadline::OneFrame => Some(Duration::from_millis(
                1_000_u64.div_ceil(u64::from(effective_fps.max(1))),
            )),
            RawFrameDeadline::Disabled => Some(Duration::ZERO),
        };
    }

    fn command_arguments(self, effective_fps: u32) -> String {
        let mut arguments = String::new();
        match self.raw_frame_deadline {
            RawFrameDeadline::Automatic => {}
            RawFrameDeadline::OneFrame => arguments.push_str(&format!(
                " --max-frame-age-ms {}",
                1_000_u64.div_ceil(u64::from(effective_fps.max(1)))
            )),
            RawFrameDeadline::Disabled => arguments.push_str(" --max-frame-age-ms 0"),
        }
        if !self.adaptive_bitrate {
            arguments.push_str(" --fixed-bitrate");
        }
        if !self.prioritize_encoding_speed {
            arguments.push_str(" --quality-priority");
        }
        arguments
    }

    fn description(self) -> String {
        let bitrate = if self.adaptive_bitrate {
            "adaptive bitrate"
        } else {
            "fixed bitrate"
        };
        let encoder = if self.prioritize_encoding_speed {
            "speed priority"
        } else {
            "quality priority"
        };
        let deadline = match self.raw_frame_deadline {
            RawFrameDeadline::Automatic => "automatic two-frame deadline",
            RawFrameDeadline::OneFrame => "one-frame deadline",
            RawFrameDeadline::Disabled => "deadline disabled",
        };
        format!("{bitrate}, {encoder}, {deadline}")
    }

    fn deviation_count(self) -> u8 {
        u8::from(!self.adaptive_bitrate)
            + u8::from(!self.prioritize_encoding_speed)
            + u8::from(self.raw_frame_deadline != RawFrameDeadline::Automatic)
    }
}

#[derive(Clone, Copy, Debug)]
struct ProfileRunResult {
    sampled_for: Duration,
    pipeline: LatencyDistribution,
    recommendations: LatencyRecommendations,
    retransmission_percent: f64,
    raw_drop_percent: f64,
    frame_rate_shortfall_percent: f64,
    measured_fps: f64,
    requested_fps: u32,
    effective_fps: u32,
    final_target_bitrate: u64,
    score_ms: f64,
}

#[derive(Debug)]
struct ProfileGroupResult {
    receivers: Vec<(IpAddr, ProfileRunResult)>,
    aggregate: ProfileRunResult,
}

struct AutoTuneTrial {
    name: &'static str,
    config: TuningConfig,
    result: ProfileRunResult,
}

fn auto_tune_profile(base: MirrorOptions, interrupted: &AtomicBool) -> Result<()> {
    if !base.synthetic {
        bail!("--auto-tune requires --synthetic so every trial receives the same workload");
    }
    if base.display_id.is_some() {
        bail!("--display cannot be combined with --synthetic or --auto-tune");
    }
    if base.extend {
        bail!("--extend cannot be combined with --synthetic or --auto-tune");
    }
    if base.max_frame_age.is_some() || !base.adaptive_bitrate || !base.prioritize_encoding_speed {
        bail!(
            "--auto-tune varies --max-frame-age-ms, --fixed-bitrate, and --quality-priority; do not set those controls explicitly"
        );
    }
    let total_duration = base
        .duration
        .expect("auto-tune duration was validated by profile_desktop");
    if total_duration < Duration::from_secs(AUTO_TUNE_MINIMUM_SECONDS) {
        bail!(
            "--auto-tune needs at least {AUTO_TUNE_MINIMUM_SECONDS} seconds so each trial covers one complete {SYNTHETIC_CYCLE_SECONDS}-second synthetic cycle"
        );
    }
    let trial_duration = total_duration / AUTO_TUNE_TRIAL_COUNT as u32;
    println!(
        "Auto-tuning latency controls with {AUTO_TUNE_TRIAL_COUNT} deterministic trials of {:.1}s each ({:.1}s measured total).",
        trial_duration.as_secs_f64(),
        trial_duration.as_secs_f64() * AUTO_TUNE_TRIAL_COUNT as f64
    );
    println!(
        "The receiver is relaunched between trials; setup time is outside the --seconds budget."
    );

    let first_five = [
        ("baseline defaults", TuningConfig::BASELINE),
        (
            "fixed bitrate",
            TuningConfig {
                adaptive_bitrate: false,
                ..TuningConfig::BASELINE
            },
        ),
        (
            "quality priority",
            TuningConfig {
                prioritize_encoding_speed: false,
                ..TuningConfig::BASELINE
            },
        ),
        (
            "one-frame deadline",
            TuningConfig {
                raw_frame_deadline: RawFrameDeadline::OneFrame,
                ..TuningConfig::BASELINE
            },
        ),
        (
            "deadline disabled",
            TuningConfig {
                raw_frame_deadline: RawFrameDeadline::Disabled,
                ..TuningConfig::BASELINE
            },
        ),
    ];
    let mut trials = Vec::with_capacity(AUTO_TUNE_TRIAL_COUNT);
    for (index, (name, config)) in first_five.into_iter().enumerate() {
        let deadline_fps = trials
            .first()
            .map(|trial: &AutoTuneTrial| trial.result.effective_fps)
            .unwrap_or(base.fps);
        trials.push(run_auto_tune_trial(
            &base,
            name,
            config,
            index,
            trial_duration,
            deadline_fps,
            interrupted,
        )?);
        if interrupted.load(Ordering::SeqCst) {
            println!(
                "Auto-tune stopped after {} of {AUTO_TUNE_TRIAL_COUNT} trials; no recommendation was generated.",
                trials.len()
            );
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }

    let combined = combined_tuning_config(
        trials[0].result.score_ms,
        trials[1].result.score_ms,
        trials[2].result.score_ms,
        trials[3].result.score_ms,
        trials[4].result.score_ms,
    );
    trials.push(run_auto_tune_trial(
        &base,
        "combined validation",
        combined,
        AUTO_TUNE_TRIAL_COUNT - 1,
        trial_duration,
        trials[0].result.effective_fps,
        interrupted,
    )?);
    if interrupted.load(Ordering::SeqCst) {
        println!("Auto-tune stopped during the final trial; no recommendation was generated.");
        return Ok(());
    }

    print_auto_tune_report(&base, &trials);
    Ok(())
}

fn run_auto_tune_trial(
    base: &MirrorOptions,
    name: &'static str,
    config: TuningConfig,
    index: usize,
    duration: Duration,
    deadline_fps: u32,
    interrupted: &AtomicBool,
) -> Result<AutoTuneTrial> {
    let mut options = base.clone();
    options.duration = Some(duration);
    config.apply(&mut options, deadline_fps);
    println!(
        "\nTrial {}/{}: {name} ({})",
        index + 1,
        AUTO_TUNE_TRIAL_COUNT,
        config.description()
    );
    let result = run_desktop(
        options,
        RunMode::Profile,
        ProfilePresentation::AutoTuneTrial,
        interrupted,
        None,
    )?
    .expect("profile mode returns profile measurements")
    .aggregate;
    println!(
        "Trial result: p95 {:.1} ms, p99 {:.1} ms, retrans {:.2}%, raw drops {:.2}%, {:.1} fps, score {:.1} ms",
        micros_to_millis(result.pipeline.p95_micros),
        micros_to_millis(result.pipeline.p99_micros),
        result.retransmission_percent,
        result.raw_drop_percent,
        result.measured_fps,
        result.score_ms
    );
    Ok(AutoTuneTrial {
        name,
        config,
        result,
    })
}

fn combined_tuning_config(
    baseline_score: f64,
    fixed_bitrate_score: f64,
    quality_priority_score: f64,
    one_frame_deadline_score: f64,
    disabled_deadline_score: f64,
) -> TuningConfig {
    let adaptive_bitrate = fixed_bitrate_score + AUTO_TUNE_NOISE_MARGIN_MS >= baseline_score;
    let prioritize_encoding_speed =
        quality_priority_score + AUTO_TUNE_NOISE_MARGIN_MS >= baseline_score;
    let (raw_frame_deadline, deadline_score) = [
        (RawFrameDeadline::OneFrame, one_frame_deadline_score),
        (RawFrameDeadline::Disabled, disabled_deadline_score),
    ]
    .into_iter()
    .min_by(|left, right| left.1.total_cmp(&right.1))
    .expect("deadline candidates are non-empty");
    let raw_frame_deadline = if deadline_score + AUTO_TUNE_NOISE_MARGIN_MS < baseline_score {
        raw_frame_deadline
    } else {
        RawFrameDeadline::Automatic
    };
    TuningConfig {
        adaptive_bitrate,
        prioritize_encoding_speed,
        raw_frame_deadline,
    }
}

fn select_tuning_winner(entries: &[(TuningConfig, f64)]) -> usize {
    let minimum_score = entries
        .iter()
        .map(|(_, score)| *score)
        .min_by(f64::total_cmp)
        .expect("auto-tune entries are non-empty");
    entries
        .iter()
        .enumerate()
        .filter(|(_, (_, score))| *score <= minimum_score + AUTO_TUNE_NOISE_MARGIN_MS)
        .min_by(|(_, left), (_, right)| {
            left.0
                .deviation_count()
                .cmp(&right.0.deviation_count())
                .then_with(|| left.1.total_cmp(&right.1))
        })
        .map(|(index, _)| index)
        .expect("at least one auto-tune entry is within the noise margin")
}

fn print_auto_tune_report(base: &MirrorOptions, trials: &[AutoTuneTrial]) {
    let entries: Vec<_> = trials
        .iter()
        .map(|trial| (trial.config, trial.result.score_ms))
        .collect();
    let winner_index = select_tuning_winner(&entries);
    let winner = &trials[winner_index];

    println!("\nAuto-tune comparison (lower score is better):");
    println!(
        "  {:<22} {:>8} {:>8} {:>9} {:>9} {:>8} {:>10}",
        "trial", "p95 ms", "p99 ms", "retrans", "raw drop", "fps", "score ms"
    );
    for (index, trial) in trials.iter().enumerate() {
        let marker = if index == winner_index { '*' } else { ' ' };
        println!(
            "{marker} {:<22} {:>8.1} {:>8.1} {:>8.2}% {:>8.2}% {:>8.1} {:>10.1}",
            trial.name,
            micros_to_millis(trial.result.pipeline.p95_micros),
            micros_to_millis(trial.result.pipeline.p99_micros),
            trial.result.retransmission_percent,
            trial.result.raw_drop_percent,
            trial.result.measured_fps,
            trial.result.score_ms,
        );
    }
    println!(
        "\nScore = p95 + 25% of the p95→p99 tail + 10 ms per retransmission percentage point + 2 ms per raw-drop percentage point + 2 ms per frame-rate shortfall percentage point."
    );
    println!(
        "Settings within {:.0} ms are treated as measurement noise, with fewer non-default controls preferred.",
        AUTO_TUNE_NOISE_MARGIN_MS
    );
    println!("Winner: {} ({})", winner.name, winner.config.description());
    if winner.result.requested_fps != winner.result.effective_fps {
        println!(
            "Receiver capability: requested {} fps, negotiated {} fps; the recommendation uses {} fps.",
            winner.result.requested_fps, winner.result.effective_fps, winner.result.effective_fps
        );
    }
    println!(
        "Winner details: {:.1}s sampled, frame-rate shortfall {:.2}%, final bitrate {:.2} Mbit/s; balanced receiver delay {} ms.",
        winner.result.sampled_for.as_secs_f64(),
        winner.result.frame_rate_shortfall_percent,
        winner.result.final_target_bitrate as f64 / 1_000_000.0,
        winner.result.recommendations.balanced_ms,
    );
    println!(
        "\nUse: cast desktop{} --cast-port {} --target-delay-ms {} --fps {} --width {} --height {} --bitrate {}{}",
        host_command_arguments(&base.cast_hosts),
        base.cast_port,
        winner.result.recommendations.balanced_ms,
        winner.result.effective_fps,
        even(base.width),
        even(base.height),
        base.bitrate,
        winner.config.command_arguments(winner.result.effective_fps),
    );
    println!(
        "This tunes sender latency and reliability under synthetic-v1; it does not measure image quality or glass-to-glass display latency. Increase --seconds to reduce trial-to-trial noise."
    );
}

fn host_command_arguments(hosts: &[IpAddr]) -> String {
    hosts.iter().map(|host| format!(" --host {host}")).collect()
}

fn run_extended_desktop(
    options: MirrorOptions,
    mode: RunMode,
    presentation: ProfilePresentation,
    interrupted: &Arc<AtomicBool>,
) -> Result<Option<ProfileGroupResult>> {
    let width = even(options.width);
    let height = even(options.height);
    let mut displays = Vec::with_capacity(options.cast_hosts.len());
    for (index, host) in options.cast_hosts.iter().copied().enumerate() {
        let ordinal = u32::try_from(index + 1)
            .context("the number of extended displays exceeded the supported ordinal range")?;
        let session = VirtualDisplaySession::start(width, height, options.fps, ordinal)?;
        println!(
            "Mapped receiver {host} to temporary extended display {ordinal} (display {}).",
            session.display_id()
        );
        displays.push((host, session));
    }

    let worker_presentation =
        if mode == RunMode::Profile && presentation == ProfilePresentation::Detailed {
            ProfilePresentation::GroupedTarget
        } else {
            presentation
        };
    let results = thread::scope(|scope| {
        let mut workers = Vec::with_capacity(displays.len());
        for (host, session) in displays {
            let mut target_options = options.clone();
            target_options.cast_hosts = vec![host];
            target_options.extend = false;
            target_options.display_id = Some(session.display_id());
            let stop = Arc::clone(interrupted);
            workers.push((
                host,
                scope.spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run_desktop(
                            target_options,
                            mode,
                            worker_presentation,
                            stop.as_ref(),
                            Some(session),
                        )
                    }))
                    .map_err(|_| anyhow!("desktop cast worker for {host} panicked"))
                    .and_then(std::convert::identity);
                    if result.is_err() {
                        stop.store(true, Ordering::SeqCst);
                    }
                    result
                }),
            ));
        }

        workers
            .into_iter()
            .map(|(host, worker)| {
                let result = worker
                    .join()
                    .map_err(|_| anyhow!("desktop cast worker for {host} panicked"))?;
                result.with_context(|| format!("desktop cast to {host} failed"))
            })
            .collect::<Vec<_>>()
    });

    let mut failures = Vec::new();
    let mut receivers = Vec::new();
    for result in results {
        match result {
            Ok(Some(group)) => receivers.extend(group.receivers),
            Ok(None) => {}
            Err(error) => failures.push(format!("{error:#}")),
        }
    }
    if !failures.is_empty() {
        bail!("multi-receiver cast stopped: {}", failures.join("; "));
    }
    if mode == RunMode::Cast {
        return Ok(None);
    }

    let aggregate = aggregate_profile_results(
        &receivers
            .iter()
            .map(|(_, result)| *result)
            .collect::<Vec<_>>(),
    )?;
    if presentation == ProfilePresentation::Detailed {
        print_group_profile_report(&options, &receivers, aggregate);
    }
    Ok(Some(ProfileGroupResult {
        receivers,
        aggregate,
    }))
}

fn run_desktop(
    mut options: MirrorOptions,
    mode: RunMode,
    presentation: ProfilePresentation,
    interrupted: &AtomicBool,
    precreated_display: Option<VirtualDisplaySession>,
) -> Result<Option<ProfileGroupResult>> {
    validate_cast_hosts(&options.cast_hosts)?;
    if options.bitrate <= 0 {
        bail!("bitrate must be greater than zero");
    }
    if options.target_delay.is_zero() || options.target_delay > Duration::from_secs(5) {
        bail!("Cast mirroring target delay must be between 1 and 5000 milliseconds");
    }
    if options.synthetic && mode != RunMode::Profile {
        bail!("synthetic frames are available only in profiling mode");
    }
    if options.synthetic && options.display_id.is_some() {
        bail!("--display cannot be combined with --synthetic");
    }
    if options.synthetic && options.audio {
        bail!("desktop audio is unavailable with synthetic profiling");
    }
    if options.extend && options.display_id.is_some() {
        bail!("--extend cannot be combined with --display");
    }
    if options.extend && options.synthetic {
        bail!("--extend cannot be combined with --synthetic");
    }
    let prepared_audio = if options.audio {
        match audio::prepare() {
            Ok(encoder) => Some(encoder),
            Err(error) => {
                eprintln!("System audio is unavailable; continuing with video only: {error:#}");
                options.audio = false;
                None
            }
        }
    } else {
        None
    };

    let width = even(options.width);
    let height = even(options.height);
    let h264_level = H264Level::for_stream(width, height, options.fps, options.bitrate as u64)?;
    log::debug!(
        "selected H.264 Baseline level {} ({}) for {}x{} at {} fps",
        h264_level.name,
        h264_level.codec_parameter,
        width,
        height,
        options.fps
    );
    let mut virtual_display = match (options.extend, precreated_display) {
        (true, Some(_)) => bail!("temporary display ownership was specified twice"),
        (true, None) => Some(VirtualDisplaySession::start(width, height, options.fps, 1)?),
        (false, display) => display,
    };
    if let Some(session) = virtual_display.as_ref() {
        options.display_id = Some(session.display_id());
    }
    let mut targets = negotiate_cast_streaming_group(
        &options.cast_hosts,
        options.cast_port,
        width,
        height,
        options.fps,
        options.bitrate as u32,
        options.target_delay,
        h264_level,
        options.audio,
    )?;
    let requested_fps = options.fps;
    let receiver_fps = negotiated_group_frame_rate(&targets, options.fps);
    if receiver_fps < options.fps {
        eprintln!(
            "A receiver selected {receiver_fps} fps instead of {} fps; capping the shared capture and encoding pipeline to the group rate.",
            options.fps
        );
        options.fps = receiver_fps;
    }

    let capture_stats = Arc::new(MirrorStats::default());
    capture_stats
        .requested_frame_rate
        .store(u64::from(requested_fps), Ordering::Relaxed);
    capture_stats
        .effective_frame_rate
        .store(u64::from(options.fps), Ordering::Relaxed);
    let rate_control = Arc::new(AdaptiveRateControl::new_group(
        options.bitrate as u64,
        options.adaptive_bitrate,
        Arc::clone(&capture_stats),
        targets.len(),
    ));
    let failure = Arc::new(Mutex::new(None));
    let mut sender_workers = Vec::with_capacity(targets.len());
    let mut feedback_threads = Vec::with_capacity(targets.len());
    let mut outputs = Vec::with_capacity(targets.len());
    let mut audio_outputs = Vec::with_capacity(targets.len());
    let mut target_stats = Vec::with_capacity(targets.len());
    for (index, target) in targets.iter().enumerate() {
        let host = target.host;
        let transport = &target.transport;
        let local_ip = local_ip_for(host, options.cast_port)?;
        let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0))
            .with_context(|| format!("could not bind Cast Streaming UDP socket on {local_ip}"))?;
        socket
            .connect(SocketAddr::new(host, transport.udp_port))
            .with_context(|| {
                format!(
                    "could not connect Cast Streaming UDP socket to {}:{}",
                    host, transport.udp_port
                )
            })?;
        log::debug!(
            "Cast Streaming UDP route for {} is {} -> {}:{}",
            host,
            socket.local_addr()?,
            host,
            transport.udp_port
        );

        let stats = if targets.len() == 1 {
            Arc::clone(&capture_stats)
        } else {
            Arc::new(MirrorStats::default())
        };
        stats
            .requested_frame_rate
            .store(u64::from(requested_fps), Ordering::Relaxed);
        stats
            .effective_frame_rate
            .store(u64::from(options.fps), Ordering::Relaxed);
        let sender = Arc::new(Mutex::new(CastRtpSender::new(
            socket.try_clone()?,
            host.is_ipv4(),
            transport.sender_ssrc,
            transport.receiver_ssrc,
            transport.aes_key,
            transport.aes_iv_mask,
            Arc::clone(&stats),
            Arc::clone(&rate_control),
            index,
            options.fps,
            options.target_delay,
        )?));
        let mut feedback_senders = vec![Arc::clone(&sender)];
        if let Some(audio_transport) = &transport.audio {
            let audio_sender = Arc::new(Mutex::new(CastRtpSender::new_audio(
                socket.try_clone()?,
                host.is_ipv4(),
                audio_transport.sender_ssrc,
                audio_transport.receiver_ssrc,
                audio_transport.aes_key,
                audio_transport.aes_iv_mask,
                options.target_delay,
            )?));
            let (audio_output, audio_sender_worker) = SenderWorker::start(
                targets.len() + index + 1,
                host,
                Arc::clone(&audio_sender),
                Arc::clone(&failure),
            )?;
            feedback_senders.push(audio_sender);
            audio_outputs.push(audio_output);
            sender_workers.push(audio_sender_worker);
        }
        let feedback = FeedbackThread::start(
            index + 1,
            host,
            socket,
            feedback_senders,
            Arc::clone(&stats),
            Arc::clone(&failure),
        )?;
        let (output, worker) = SenderWorker::start(index + 1, host, sender, Arc::clone(&failure))?;
        target_stats.push((host, stats));
        feedback_threads.push(feedback);
        sender_workers.push(worker);
        outputs.push(output);
    }

    let keyframe_interval = i32::try_from(options.fps)
        .context("frame rate exceeded VideoToolbox's keyframe interval range")?;
    let encoder = CompressionSession::builder(width as i32, height as i32, Codec::H264)
        .with_real_time(true)
        .with_allow_frame_reordering(false)
        .with_average_bit_rate(options.bitrate)
        .with_expected_frame_rate(options.fps as f64)
        .with_max_keyframe_interval(keyframe_interval)
        .with_profile_level(h264_level.profile_level)
        .build()
        .context("could not create the VideoToolbox H.264 mirroring encoder")?;
    configure_low_latency_encoder(&encoder, options.prioritize_encoding_speed, &capture_stats);

    let clock = Arc::new(MediaClock::default());
    let pipeline = MirrorPipeline {
        encoder,
        outputs,
        parameter_sets: None,
        frame_index: 0,
        clock: Arc::clone(&clock),
        last_timestamp: None,
        fps: options.fps,
        rate_control,
        encoder_bitrate: options.bitrate,
        stats: Arc::clone(&capture_stats),
    };
    let (audio_submitter, audio_worker) = if !audio_outputs.is_empty() {
        let (submitter, worker) = AudioWorker::start_prepared(
            prepared_audio.expect("audio was prepared before it was offered"),
            Arc::clone(&clock),
            Arc::clone(&failure),
            move |audio_frame| {
                let frame = EncodedFrame {
                    rtp_timestamp: audio_frame.timestamp as u32,
                    keyframe: true,
                    data: Arc::new(audio_frame.data),
                    timings: FrameTimings {
                        pipeline_started_at: Instant::now(),
                        capture_age_micros: None,
                        queue_wait_micros: 0,
                        encode_micros: 0,
                        prepare_micros: 0,
                        sender_lock_wait_micros: 0,
                    },
                    synthetic_phase: None,
                };
                for output in &audio_outputs {
                    output.submit(frame.clone())?;
                }
                Ok(())
            },
        )
        .context("could not start the desktop audio encoder after receiver negotiation")?;
        (Some(submitter), Some(worker))
    } else {
        (None, None)
    };
    let max_frame_age = resolve_max_frame_age(options.max_frame_age, options.fps);
    let (submitter, encoder_worker) = EncoderWorker::start(
        pipeline,
        max_frame_age,
        Arc::clone(&failure),
        Arc::clone(&capture_stats),
    )?;
    if let Some(max_frame_age) = max_frame_age {
        log::debug!(
            "latest-frame-wins queue enabled with {:.1} ms raw-frame deadline",
            max_frame_age.as_secs_f64() * 1_000.0
        );
    } else {
        log::debug!("latest-frame-wins queue enabled without a raw-frame deadline");
    }
    let volume_controls = targets
        .iter()
        .filter(|target| target.transport.audio.is_some())
        .map(|target| Arc::clone(&target.transport.control))
        .collect::<Vec<_>>();
    let local_output_redirect = if audio_submitter.is_some() {
        match LocalOutputRedirect::start(move |control| {
            let command = match control {
                LocalOutputControl::Volume(level) => ReceiverVolumeCommand::SetLevel(level),
                LocalOutputControl::ToggleMute => ReceiverVolumeCommand::ToggleMute,
            };
            for target in &volume_controls {
                target.request_volume(command);
            }
        }) {
            Ok(redirect) => Some(redirect),
            Err(error) => {
                eprintln!(
                    "Could not suppress local desktop audio or route volume controls to the receiver: {error:#}"
                );
                None
            }
        }
    } else {
        None
    };
    let input = if options.synthetic {
        println!(
            "Profiling {SYNTHETIC_WORKLOAD_NAME} deterministic 420v frames at {}x{}, {} fps, probe delay {} ms...",
            width,
            height,
            options.fps,
            options.target_delay.as_millis()
        );
        RunningInput::Synthetic(
            SyntheticFrameSource::start(
                submitter,
                width,
                height,
                options.fps,
                Arc::clone(&failure),
                Arc::clone(&capture_stats),
            )?,
            encoder_worker,
        )
    } else {
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
        let source_width = display.width();
        let source_height = display.height();
        log::debug!(
            "scaling source display {source_width}x{source_height} into H.264 mirroring output {width}x{height}"
        );

        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build();
        let frame_interval = CMTime::new(1, options.fps as i32);
        let config = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_scales_to_fit(true)
            .with_preserves_aspect_ratio(true)
            .with_pixel_format(PixelFormat::YCbCr_420v)
            .with_shows_cursor(true)
            .with_queue_depth(3)
            .with_minimum_frame_interval(&frame_interval);
        let config = if audio_submitter.is_some() {
            config
                .with_captures_audio(true)
                .with_sample_rate(audio::SAMPLE_RATE as i32)
                .with_channel_count(audio::CHANNELS as i32)
                .with_excludes_current_process_audio(true)
        } else {
            config
        };
        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(
            MirrorFrameHandler {
                submitter,
                failure: Arc::clone(&failure),
                last_surface: Mutex::new(None),
                repeated_samples: AtomicU64::new(0),
                skipped_samples: AtomicU64::new(0),
            },
            SCStreamOutputType::Screen,
        );
        if let Some(audio_submitter) = audio_submitter {
            stream.add_output_handler(
                AudioFrameHandler::new(audio_submitter),
                SCStreamOutputType::Audio,
            );
        }

        match mode {
            RunMode::Cast => println!(
                "Mirroring display {} at {}x{} into {}x{}, {} fps, target delay {} ms...",
                display.display_id(),
                source_width,
                source_height,
                width,
                height,
                options.fps,
                options.target_delay.as_millis()
            ),
            RunMode::Profile => println!(
                "Profiling display {} at {}x{} into {}x{}, {} fps, probe delay {} ms...",
                display.display_id(),
                source_width,
                source_height,
                width,
                height,
                options.fps,
                options.target_delay.as_millis()
            ),
        }
        stream
            .start_capture()
            .context("could not start screen capture")?;
        RunningInput::Screen(stream, encoder_worker, audio_worker)
    };
    if mode == RunMode::Cast {
        println!(
            "Casting desktop over the low-latency Cast Streaming transport. Press Ctrl-C to stop."
        );
    } else if options.synthetic {
        println!(
            "Synthetic cycle: static → partial motion → full motion → scene cuts every {SYNTHETIC_CYCLE_SECONDS}s. Press Ctrl-C to finish early."
        );
    } else {
        println!("Use the desktop normally during the sample. Press Ctrl-C to finish early.");
    }

    let started = Instant::now();
    let graph_stats = target_stats.first().map(|(_, stats)| Arc::clone(stats));
    let mut graph = (mode == RunMode::Profile
        && target_stats.len() == 1
        && presentation != ProfilePresentation::GroupedTarget)
        .then(|| {
            LiveLatencyGraph::new(
                options.duration.expect("profile duration was validated"),
                options.synthetic,
                options.fps,
            )
        });
    let mut run_result = (|| -> Result<()> {
        loop {
            if interrupted.load(Ordering::SeqCst)
                || options
                    .duration
                    .is_some_and(|duration| started.elapsed() >= duration)
            {
                break;
            }
            if let Some(error) = take_failure(&failure)? {
                bail!("mirroring pipeline failed: {error}");
            }
            for target in &targets {
                target.ensure_alive()?;
            }
            if let Some(session) = virtual_display.as_mut() {
                session.ensure_alive()?;
            }
            if let (Some(graph), Some(stats)) = (graph.as_mut(), graph_stats.as_deref()) {
                graph.draw_if_due(stats, started.elapsed())?;
            }
            thread::sleep(Duration::from_millis(50));
        }

        if let (Some(graph), Some(stats)) = (graph.as_mut(), graph_stats.as_deref()) {
            graph.finish(stats, started.elapsed())?;
        }
        Ok(())
    })();
    let sampled_for = started.elapsed();
    if run_result.is_err() {
        interrupted.store(true, Ordering::SeqCst);
    }

    let input_result = input.stop_and_release();
    let output_result = local_output_redirect.map_or(Ok(()), LocalOutputRedirect::stop);
    // Stopping an SCStream does not release the stream or its content filter.
    // Drop the capture graph before removing a virtual source display so
    // ScreenCaptureKit cannot keep that display registered with WindowServer.
    log::debug!("released desktop capture resources before display teardown");
    let mut sender_result = Ok(());
    for worker in &mut sender_workers {
        if let Err(error) = worker.stop()
            && sender_result.is_ok()
        {
            sender_result = Err(error);
        }
    }
    drop(feedback_threads);
    if run_result.is_ok() {
        run_result = match take_failure(&failure) {
            Ok(Some(error)) => Err(anyhow!("mirroring pipeline failed: {error}")),
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        };
    }
    if run_result.is_err()
        || input_result.is_err()
        || output_result.is_err()
        || sender_result.is_err()
    {
        interrupted.store(true, Ordering::SeqCst);
    }
    for target in &mut targets {
        if let Err(error) = target.stop() {
            log::warn!("could not stop a Cast Streaming receiver cleanly: {error:#}");
        }
    }

    let merge_result = (|| -> Result<()> {
        for (_, stats) in &target_stats {
            if !Arc::ptr_eq(&capture_stats, stats) {
                merge_capture_stats(&capture_stats, stats)?;
            }
        }
        Ok(())
    })();
    let profile_result = (|| -> Result<Option<ProfileGroupResult>> {
        merge_result?;
        match mode {
            RunMode::Cast => {
                for (host, stats) in &target_stats {
                    print_cast_summary(*host, stats);
                }
                Ok(None)
            }
            RunMode::Profile => {
                let mut receivers = Vec::with_capacity(target_stats.len());
                let grouped =
                    target_stats.len() > 1 || presentation == ProfilePresentation::GroupedTarget;
                for (host, stats) in &target_stats {
                    let result = collect_profile_result(stats, sampled_for)?;
                    if presentation != ProfilePresentation::AutoTuneTrial {
                        print_profile_report(stats, sampled_for, &options, *host, !grouped)?;
                    }
                    receivers.push((*host, result));
                }
                let aggregate = aggregate_profile_results(
                    &receivers
                        .iter()
                        .map(|(_, result)| *result)
                        .collect::<Vec<_>>(),
                )?;
                if presentation == ProfilePresentation::Detailed && grouped {
                    print_group_profile_report(&options, &receivers, aggregate);
                }
                Ok(Some(ProfileGroupResult {
                    receivers,
                    aggregate,
                }))
            }
        }
    })();
    let display_result = if let Some(session) = virtual_display.as_mut() {
        let _teardown = VIRTUAL_DISPLAY_TEARDOWN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        session.stop()
    } else {
        Ok(())
    };

    run_result?;
    input_result?;
    output_result?;
    sender_result?;
    display_result?;
    profile_result
}

fn print_cast_summary(host: IpAddr, stats: &MirrorStats) {
    println!("Receiver {host}:");
    println!(
        "Stopped. Sent {} frames in {} RTP packets ({} retransmissions); received {} RTCP feedback packets.",
        stats.frames.load(Ordering::Relaxed),
        stats.rtp_packets.load(Ordering::Relaxed),
        stats.retransmissions.load(Ordering::Relaxed),
        stats.feedback_packets.load(Ordering::Relaxed)
    );
    let requested_fps = stats.requested_frame_rate.load(Ordering::Relaxed);
    let effective_fps = stats.effective_frame_rate.load(Ordering::Relaxed);
    if requested_fps != effective_fps {
        println!(
            "Receiver capability capped the requested {requested_fps} fps stream to {effective_fps} fps."
        );
    }
    let checkpoint_samples = stats.checkpoint_samples.load(Ordering::Relaxed);
    if let Some(average_micros) = stats
        .checkpoint_latency_micros
        .load(Ordering::Relaxed)
        .checked_div(checkpoint_samples)
    {
        println!(
            "Receiver completed frames in {:.1} ms on average ({:.1} ms maximum); reported playout delay: {} ms.",
            micros_to_millis(average_micros),
            micros_to_millis(stats.max_checkpoint_latency_micros.load(Ordering::Relaxed)),
            stats.receiver_playout_delay_ms.load(Ordering::Relaxed)
        );
    }
    println!(
        "Raw-frame queue: {} submitted, {} replaced by newer frames, {} expired; peak network backlog: {} frames / {:.1} KiB.",
        stats.raw_frames_submitted.load(Ordering::Relaxed),
        stats.raw_frames_replaced.load(Ordering::Relaxed),
        stats.raw_frames_expired.load(Ordering::Relaxed),
        stats.max_in_flight_frames.load(Ordering::Relaxed),
        stats.max_in_flight_bytes.load(Ordering::Relaxed) as f64 / 1024.0,
    );
    println!(
        "Adaptive bitrate ended at {:.2} Mbit/s (minimum {:.2}; {} decreases, {} increases); bounded packet pacing slept {:.1} ms total.",
        stats.current_target_bitrate.load(Ordering::Relaxed) as f64 / 1_000_000.0,
        stats.minimum_target_bitrate.load(Ordering::Relaxed) as f64 / 1_000_000.0,
        stats.adaptive_bitrate_decreases.load(Ordering::Relaxed),
        stats.adaptive_bitrate_increases.load(Ordering::Relaxed),
        stats.pacing_sleep_micros.load(Ordering::Relaxed) as f64 / 1_000.0,
    );
}

#[derive(Clone, Copy, Debug)]
struct LatencyDistribution {
    count: usize,
    average_micros: u64,
    p50_micros: u64,
    p95_micros: u64,
    p99_micros: u64,
    max_micros: u64,
}

impl LatencyDistribution {
    fn from_values(mut values: Vec<u64>) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        values.sort_unstable();
        let total: u128 = values.iter().map(|value| u128::from(*value)).sum();
        let average = total / values.len() as u128;
        Some(Self {
            count: values.len(),
            average_micros: u64::try_from(average).unwrap_or(u64::MAX),
            p50_micros: percentile(&values, 50),
            p95_micros: percentile(&values, 95),
            p99_micros: percentile(&values, 99),
            max_micros: *values.last().expect("non-empty latency values"),
        })
    }
}

fn percentile(sorted: &[u64], percentage: usize) -> u64 {
    debug_assert!(!sorted.is_empty());
    debug_assert!((1..=100).contains(&percentage));
    let rank = percentage
        .saturating_mul(sorted.len())
        .div_ceil(100)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[rank]
}

struct LiveLatencyGraph {
    duration: Duration,
    synthetic: bool,
    fps: u32,
    next_draw: Duration,
    last_sample_count: usize,
    history: VecDeque<Option<u64>>,
    terminal: bool,
    rendered: bool,
}

impl LiveLatencyGraph {
    fn new(duration: Duration, synthetic: bool, fps: u32) -> Self {
        Self {
            duration,
            synthetic,
            fps,
            next_draw: Duration::from_secs(1),
            last_sample_count: 0,
            history: VecDeque::with_capacity(PROFILE_HISTORY_SECONDS),
            terminal: io::stdout().is_terminal(),
            rendered: false,
        }
    }

    fn draw_if_due(&mut self, stats: &MirrorStats, elapsed: Duration) -> Result<()> {
        if elapsed < self.next_draw {
            return Ok(());
        }
        self.next_draw = Duration::from_secs(elapsed.as_secs().saturating_add(1));
        self.draw(stats, elapsed)
    }

    fn finish(&mut self, stats: &MirrorStats, elapsed: Duration) -> Result<()> {
        self.draw(stats, elapsed)?;
        if self.terminal {
            println!();
        }
        Ok(())
    }

    fn draw(&mut self, stats: &MirrorStats, elapsed: Duration) -> Result<()> {
        let samples = latency_samples(stats)?;
        let interval_p95 = samples
            .get(self.last_sample_count..)
            .and_then(|new| {
                LatencyDistribution::from_values(
                    new.iter().map(|sample| sample.pipeline_micros).collect(),
                )
            })
            .map(|distribution| micros_to_millis_ceil(distribution.p95_micros));
        self.last_sample_count = samples.len();
        self.history.push_back(interval_p95);
        while self.history.len() > PROFILE_HISTORY_SECONDS {
            self.history.pop_front();
        }

        let overall = LatencyDistribution::from_values(
            samples
                .iter()
                .map(|sample| sample.pipeline_micros)
                .collect(),
        );
        let (sparkline, scale_ms) = self.sparkline();
        let elapsed_seconds = elapsed.as_secs().min(self.duration.as_secs());
        let duration_seconds = self.duration.as_secs();
        let progress = progress_bar(elapsed, self.duration, 30);
        let phase = self.synthetic.then(|| {
            phase_for_frame(frame_at_elapsed(elapsed, self.fps), self.fps)
                .0
                .label()
        });
        let phase_suffix = phase.map_or_else(String::new, |phase| format!(" | {phase}"));
        let latency_label = if self.synthetic {
            "frame-ready→ACK"
        } else {
            "callback→ACK"
        };
        let stats_line = overall.map_or_else(
            || "waiting for receiver checkpoints...".to_owned(),
            |distribution| {
                format!(
                    "{latency_label} p50 {:>5.1} p95 {:>5.1} p99 {:>5.1} ms | loss {:>5.2}% | drop {} | flight {}f/{:.0}K | {:.1}M",
                    micros_to_millis(distribution.p50_micros),
                    micros_to_millis(distribution.p95_micros),
                    micros_to_millis(distribution.p99_micros),
                    retransmission_percent(stats),
                    stats.raw_frames_replaced.load(Ordering::Relaxed)
                        + stats.raw_frames_expired.load(Ordering::Relaxed),
                    stats.current_in_flight_frames.load(Ordering::Relaxed),
                    stats.current_in_flight_bytes.load(Ordering::Relaxed) as f64 / 1024.0,
                    stats.current_target_bitrate.load(Ordering::Relaxed) as f64 / 1_000_000.0,
                )
            },
        );

        if self.terminal {
            if self.rendered {
                print!("\x1b[3A");
            }
            println!(
                "\r\x1b[2KProfiling [{progress}] {elapsed_seconds:>3}/{duration_seconds}s{phase_suffix}"
            );
            println!("\r\x1b[2Krecent p95 [{sparkline}]  0–{scale_ms} ms");
            println!("\r\x1b[2K{stats_line}");
            io::stdout()
                .flush()
                .context("could not draw latency graph")?;
            self.rendered = true;
        } else {
            println!(
                "profile {elapsed_seconds:>3}/{duration_seconds}s{phase_suffix} | {stats_line} | p95 now {}",
                interval_p95.map_or_else(|| "n/a".to_owned(), |value| format!("{value} ms"))
            );
        }
        Ok(())
    }

    fn sparkline(&self) -> (String, u64) {
        const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let maximum = self.history.iter().flatten().copied().max().unwrap_or(0);
        let scale_ms = maximum.max(10).div_ceil(25) * 25;
        let visible = self.history.len().min(PROFILE_GRAPH_COLUMNS);
        let mut graph = " ".repeat(PROFILE_GRAPH_COLUMNS - visible);
        for value in self.history.iter().skip(self.history.len() - visible) {
            match value {
                Some(value) => {
                    let level = value
                        .saturating_mul((BLOCKS.len() - 1) as u64)
                        .div_ceil(scale_ms)
                        .min((BLOCKS.len() - 1) as u64) as usize;
                    graph.push(BLOCKS[level]);
                }
                None => graph.push('·'),
            }
        }
        (graph, scale_ms)
    }
}

fn progress_bar(elapsed: Duration, duration: Duration, width: usize) -> String {
    let total = duration.as_millis().max(1);
    let filled = elapsed.as_millis().min(total).saturating_mul(width as u128) / total;
    let filled = usize::try_from(filled).unwrap_or(width).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

fn latency_samples(stats: &MirrorStats) -> Result<Vec<LatencySample>> {
    Ok(stats
        .latency_samples
        .lock()
        .map_err(|_| anyhow!("latency sample lock was poisoned"))?
        .clone())
}

fn stage_distribution(
    samples: &[LatencySample],
    value: impl Fn(&LatencySample) -> u64,
) -> LatencyDistribution {
    LatencyDistribution::from_values(samples.iter().map(value).collect())
        .expect("the profile has at least one latency sample")
}

const fn applied_label(applied: bool) -> &'static str {
    if applied { "applied" } else { "unavailable" }
}

fn collect_profile_result(stats: &MirrorStats, sampled_for: Duration) -> Result<ProfileRunResult> {
    let samples = latency_samples(stats)?;
    let pipeline = LatencyDistribution::from_values(
        samples
            .iter()
            .map(|sample| sample.pipeline_micros)
            .collect(),
    )
    .ok_or_else(|| anyhow!("profile completed without receiver latency samples"))?;
    let retransmission_percent = retransmission_percent(stats);
    let submitted = stats.raw_frames_submitted.load(Ordering::Relaxed);
    let raw_drops = stats
        .raw_frames_replaced
        .load(Ordering::Relaxed)
        .saturating_add(stats.raw_frames_expired.load(Ordering::Relaxed))
        .min(submitted);
    let raw_drop_percent = percentage(raw_drops, submitted);
    let seconds = sampled_for.as_secs_f64().max(f64::EPSILON);
    let measured_fps = stats.frames.load(Ordering::Relaxed) as f64 / seconds;
    let requested_fps =
        u32::try_from(stats.requested_frame_rate.load(Ordering::Relaxed)).unwrap_or(u32::MAX);
    let effective_fps = u32::try_from(stats.effective_frame_rate.load(Ordering::Relaxed))
        .unwrap_or(u32::MAX)
        .max(1);
    let frame_rate_shortfall_percent =
        ((f64::from(effective_fps) - measured_fps).max(0.0) / f64::from(effective_fps)) * 100.0;
    let score_ms = auto_tune_score(
        pipeline,
        retransmission_percent,
        raw_drop_percent,
        frame_rate_shortfall_percent,
    );
    let recommendations = recommend_latency(
        pipeline,
        effective_fps,
        stats.retransmissions.load(Ordering::Relaxed),
        stats.rtp_packets.load(Ordering::Relaxed),
    );
    Ok(ProfileRunResult {
        sampled_for,
        pipeline,
        recommendations,
        retransmission_percent,
        raw_drop_percent,
        frame_rate_shortfall_percent,
        measured_fps,
        requested_fps,
        effective_fps,
        final_target_bitrate: stats.current_target_bitrate.load(Ordering::Relaxed),
        score_ms,
    })
}

fn aggregate_profile_results(results: &[ProfileRunResult]) -> Result<ProfileRunResult> {
    let Some(mut aggregate) = results.first().copied() else {
        bail!("profile group did not produce any receiver measurements");
    };
    for result in &results[1..] {
        aggregate.sampled_for = aggregate.sampled_for.min(result.sampled_for);
        aggregate.pipeline = LatencyDistribution {
            count: aggregate.pipeline.count.min(result.pipeline.count),
            average_micros: aggregate
                .pipeline
                .average_micros
                .max(result.pipeline.average_micros),
            p50_micros: aggregate
                .pipeline
                .p50_micros
                .max(result.pipeline.p50_micros),
            p95_micros: aggregate
                .pipeline
                .p95_micros
                .max(result.pipeline.p95_micros),
            p99_micros: aggregate
                .pipeline
                .p99_micros
                .max(result.pipeline.p99_micros),
            max_micros: aggregate
                .pipeline
                .max_micros
                .max(result.pipeline.max_micros),
        };
        aggregate.recommendations = LatencyRecommendations {
            aggressive_ms: aggregate
                .recommendations
                .aggressive_ms
                .max(result.recommendations.aggressive_ms),
            balanced_ms: aggregate
                .recommendations
                .balanced_ms
                .max(result.recommendations.balanced_ms),
            resilient_ms: aggregate
                .recommendations
                .resilient_ms
                .max(result.recommendations.resilient_ms),
        };
        aggregate.retransmission_percent = aggregate
            .retransmission_percent
            .max(result.retransmission_percent);
        aggregate.raw_drop_percent = aggregate.raw_drop_percent.max(result.raw_drop_percent);
        aggregate.frame_rate_shortfall_percent = aggregate
            .frame_rate_shortfall_percent
            .max(result.frame_rate_shortfall_percent);
        aggregate.measured_fps = aggregate.measured_fps.min(result.measured_fps);
        aggregate.requested_fps = aggregate.requested_fps.max(result.requested_fps);
        aggregate.effective_fps = aggregate.effective_fps.min(result.effective_fps);
        aggregate.final_target_bitrate = aggregate
            .final_target_bitrate
            .min(result.final_target_bitrate);
        aggregate.score_ms = aggregate.score_ms.max(result.score_ms);
    }
    Ok(aggregate)
}

fn print_group_profile_report(
    options: &MirrorOptions,
    receivers: &[(IpAddr, ProfileRunResult)],
    aggregate: ProfileRunResult,
) {
    println!("\nReceiver-group profile summary (worst receiver governs):");
    println!(
        "  {:<39} {:>8} {:>8} {:>9} {:>8} {:>9}",
        "receiver", "p95 ms", "p99 ms", "retrans", "fps", "balanced"
    );
    for (host, result) in receivers {
        println!(
            "  {:<39} {:>8.1} {:>8.1} {:>8.2}% {:>8.1} {:>7}ms",
            host,
            micros_to_millis(result.pipeline.p95_micros),
            micros_to_millis(result.pipeline.p99_micros),
            result.retransmission_percent,
            result.measured_fps,
            result.recommendations.balanced_ms,
        );
    }
    println!(
        "  Common receiver playout delay: aggressive {} ms, balanced {} ms, resilient {} ms.",
        aggregate.recommendations.aggressive_ms,
        aggregate.recommendations.balanced_ms,
        aggregate.recommendations.resilient_ms,
    );
    println!(
        "\nUse: cast desktop{} --cast-port {}{} --target-delay-ms {} --fps {} --width {} --height {} --bitrate {}",
        host_command_arguments(&options.cast_hosts),
        options.cast_port,
        source_command_argument(options.extend, options.display_id),
        aggregate.recommendations.balanced_ms,
        aggregate.effective_fps,
        even(options.width),
        even(options.height),
        options.bitrate,
    );
    println!(
        "The common recommendation protects the worst observed receiver; it is not a glass-to-glass display measurement."
    );
}

fn percentage(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

fn auto_tune_score(
    pipeline: LatencyDistribution,
    retransmission_percent: f64,
    raw_drop_percent: f64,
    frame_rate_shortfall_percent: f64,
) -> f64 {
    let p95_ms = micros_to_millis(pipeline.p95_micros);
    let tail_ms = micros_to_millis(pipeline.p99_micros.saturating_sub(pipeline.p95_micros));
    p95_ms
        + tail_ms * 0.25
        + retransmission_percent * 10.0
        + raw_drop_percent * 2.0
        + frame_rate_shortfall_percent * 2.0
}

fn print_profile_report(
    stats: &MirrorStats,
    sampled_for: Duration,
    options: &MirrorOptions,
    host: IpAddr,
    include_recommendation: bool,
) -> Result<()> {
    let samples = latency_samples(stats)?;
    let pipeline = LatencyDistribution::from_values(
        samples
            .iter()
            .map(|sample| sample.pipeline_micros)
            .collect(),
    )
    .ok_or_else(|| anyhow!("profile completed without receiver latency samples"))?;
    let transport = LatencyDistribution::from_values(
        samples
            .iter()
            .map(|sample| sample.transport_micros)
            .collect(),
    )
    .expect("pipeline and transport sample counts match");
    let processing = LatencyDistribution::from_values(
        samples
            .iter()
            .map(|sample| {
                sample
                    .pipeline_micros
                    .saturating_sub(sample.transport_micros)
            })
            .collect(),
    )
    .expect("pipeline and processing sample counts match");
    let capture_age = LatencyDistribution::from_values(
        samples
            .iter()
            .filter_map(|sample| sample.capture_age_micros)
            .collect(),
    );
    let composited_to_ack = LatencyDistribution::from_values(
        samples
            .iter()
            .filter_map(|sample| {
                sample
                    .capture_age_micros
                    .map(|age| age.saturating_add(sample.pipeline_micros))
            })
            .collect(),
    );
    let timestamped_callback_to_ack = LatencyDistribution::from_values(
        samples
            .iter()
            .filter(|sample| sample.capture_age_micros.is_some())
            .map(|sample| sample.pipeline_micros)
            .collect(),
    );
    let queue_wait = stage_distribution(&samples, |sample| sample.queue_wait_micros);
    let encode = stage_distribution(&samples, |sample| sample.encode_micros);
    let prepare = stage_distribution(&samples, |sample| sample.prepare_micros);
    let sender_lock = stage_distribution(&samples, |sample| sample.sender_lock_wait_micros);
    let send = stage_distribution(&samples, |sample| sample.send_micros);
    let keyframes = LatencyDistribution::from_values(
        samples
            .iter()
            .filter(|sample| sample.keyframe)
            .map(|sample| sample.pipeline_micros)
            .collect(),
    );
    let largest_keyframe_bytes = samples
        .iter()
        .filter(|sample| sample.keyframe)
        .map(|sample| sample.encoded_bytes)
        .max()
        .unwrap_or(0);
    let recommendations = recommend_latency(
        pipeline,
        options.fps,
        stats.retransmissions.load(Ordering::Relaxed),
        stats.rtp_packets.load(Ordering::Relaxed),
    );
    let frames = stats.frames.load(Ordering::Relaxed);
    let seconds = sampled_for.as_secs_f64().max(f64::EPSILON);
    let measured_fps = frames as f64 / seconds;
    let average_mbps =
        stats.encoded_bytes.load(Ordering::Relaxed) as f64 * 8.0 / seconds / 1_000_000.0;

    println!("Latency profile complete for receiver {host}");
    if options.synthetic {
        println!(
            "  Workload: {SYNTHETIC_WORKLOAD_NAME}, deterministic {SYNTHETIC_CYCLE_SECONDS}s cycle (static, partial motion, full motion, scene cuts)"
        );
    }
    let requested_fps = stats.requested_frame_rate.load(Ordering::Relaxed);
    let effective_fps = stats.effective_frame_rate.load(Ordering::Relaxed);
    if requested_fps != effective_fps {
        println!(
            "  Receiver capability: requested {requested_fps} fps, negotiated {effective_fps} fps; the profile used {effective_fps} fps."
        );
    }
    println!(
        "  Sample: {:.1}s, {} acknowledged frames ({measured_fps:.1} fps), {} keyframes",
        sampled_for.as_secs_f64(),
        pipeline.count,
        stats.keyframes.load(Ordering::Relaxed)
    );
    let pipeline_label = if options.synthetic {
        "Synthetic frame ready→receiver ACK"
    } else {
        "Capture callback→receiver ACK"
    };
    println!(
        "  {pipeline_label}: avg {:.1} ms, p50 {:.1} ms, p95 {:.1} ms, p99 {:.1} ms, max {:.1} ms",
        micros_to_millis(pipeline.average_micros),
        micros_to_millis(pipeline.p50_micros),
        micros_to_millis(pipeline.p95_micros),
        micros_to_millis(pipeline.p99_micros),
        micros_to_millis(pipeline.max_micros)
    );
    let processing_label = if options.synthetic {
        "Frame-ready/encode path"
    } else {
        "Capture/encode path"
    };
    println!(
        "  {processing_label}: p95 {:.1} ms; RTP/receiver feedback: p95 {:.1} ms",
        micros_to_millis(processing.p95_micros),
        micros_to_millis(transport.p95_micros)
    );
    if let (Some(capture_age), Some(composited_to_ack), Some(timestamped_callback_to_ack)) =
        (capture_age, composited_to_ack, timestamped_callback_to_ack)
    {
        println!(
            "  Screen timing ({} timestamped frames): composite→callback p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms; callback→ACK p95 {:.1} ms; composite→ACK p95 {:.1} ms",
            capture_age.count,
            micros_to_millis(capture_age.p50_micros),
            micros_to_millis(capture_age.p95_micros),
            micros_to_millis(capture_age.max_micros),
            micros_to_millis(timestamped_callback_to_ack.p95_micros),
            micros_to_millis(composited_to_ack.p95_micros),
        );
    }
    println!(
        "  Stage p95: raw queue {:.1} ms | VideoToolbox {:.1} ms | H.264 prepare {:.1} ms | sender-lock {:.1} ms | UDP send {:.1} ms | ACK {:.1} ms",
        micros_to_millis(queue_wait.p95_micros),
        micros_to_millis(encode.p95_micros),
        micros_to_millis(prepare.p95_micros),
        micros_to_millis(sender_lock.p95_micros),
        micros_to_millis(send.p95_micros),
        micros_to_millis(transport.p95_micros),
    );
    if let Some(keyframes) = keyframes {
        println!(
            "  Keyframes: p95 {:.1} ms, max {:.1} ms; largest frame {:.1} KiB",
            micros_to_millis(keyframes.p95_micros),
            micros_to_millis(keyframes.max_micros),
            largest_keyframe_bytes as f64 / 1024.0
        );
    }
    println!(
        "  Media rate: {average_mbps:.2} Mbit/s; retransmissions: {} of {} RTP packets ({:.3}%)",
        stats.retransmissions.load(Ordering::Relaxed),
        stats.rtp_packets.load(Ordering::Relaxed),
        retransmission_percent(stats)
    );
    println!(
        "  Raw queue: {} submitted, {} replaced, {} deadline-expired; peak in-flight: {} frames / {:.1} KiB; history evictions: {}",
        stats.raw_frames_submitted.load(Ordering::Relaxed),
        stats.raw_frames_replaced.load(Ordering::Relaxed),
        stats.raw_frames_expired.load(Ordering::Relaxed),
        stats.max_in_flight_frames.load(Ordering::Relaxed),
        stats.max_in_flight_bytes.load(Ordering::Relaxed) as f64 / 1024.0,
        stats.history_evictions.load(Ordering::Relaxed),
    );
    let sample_peak_frames = samples
        .iter()
        .map(|sample| sample.in_flight_frames)
        .max()
        .unwrap_or(0);
    let sample_peak_bytes = samples
        .iter()
        .map(|sample| sample.in_flight_bytes)
        .max()
        .unwrap_or(0);
    println!(
        "  ACKed-frame backlog snapshot peak: {sample_peak_frames} frames / {:.1} KiB; packet pacing: {:.1} ms total, {:.1} ms max/frame",
        sample_peak_bytes as f64 / 1024.0,
        stats.pacing_sleep_micros.load(Ordering::Relaxed) as f64 / 1_000.0,
        stats.max_pacing_sleep_micros.load(Ordering::Relaxed) as f64 / 1_000.0,
    );
    println!(
        "  Bitrate target: {:.2} Mbit/s requested, {:.2} minimum, {:.2} final ({} down / {} up, {} apply failures)",
        options.bitrate as f64 / 1_000_000.0,
        stats.minimum_target_bitrate.load(Ordering::Relaxed) as f64 / 1_000_000.0,
        stats.current_target_bitrate.load(Ordering::Relaxed) as f64 / 1_000_000.0,
        stats.adaptive_bitrate_decreases.load(Ordering::Relaxed),
        stats.adaptive_bitrate_increases.load(Ordering::Relaxed),
        stats
            .adaptive_bitrate_apply_failures
            .load(Ordering::Relaxed),
    );
    println!(
        "  VideoToolbox controls: MaxFrameDelayCount=0 {}, speed-priority {}.",
        if stats.vt_max_frame_delay_applied.load(Ordering::Relaxed) {
            "applied"
        } else {
            "unsupported (per-frame completion active)"
        },
        if options.prioritize_encoding_speed {
            applied_label(stats.vt_speed_priority_applied.load(Ordering::Relaxed))
        } else {
            "disabled by --quality-priority"
        },
    );
    println!(
        "  Receiver-reported playout delay at the end of the profile: {} ms.",
        stats.receiver_playout_delay_ms.load(Ordering::Relaxed)
    );
    if options.synthetic {
        print_synthetic_breakdown(stats, &samples)?;
    }

    if !include_recommendation {
        return Ok(());
    }

    println!("\nRecommended receiver playout delay:");
    println!(
        "  Aggressive: {:>4} ms  (covers the measured p95 with minimal decode margin)",
        recommendations.aggressive_ms
    );
    println!(
        "  Balanced:   {:>4} ms  (p99 + one frame + observed loss margin) ← recommended",
        recommendations.balanced_ms
    );
    println!(
        "  Resilient:  {:>4} ms  (covers the observed maximum with extra headroom)",
        recommendations.resilient_ms
    );
    let mut tuning_arguments = String::new();
    if let Some(max_frame_age) = options.max_frame_age {
        tuning_arguments.push_str(&format!(
            " --max-frame-age-ms {}",
            max_frame_age.as_millis()
        ));
    }
    if !options.adaptive_bitrate {
        tuning_arguments.push_str(" --fixed-bitrate");
    }
    if !options.prioritize_encoding_speed {
        tuning_arguments.push_str(" --quality-priority");
    }
    let source_argument = source_command_argument(options.extend, options.display_id);
    println!(
        "\nUse: cast desktop{} --cast-port {}{} --target-delay-ms {} --fps {} --width {} --height {} --bitrate {}{}",
        host_command_arguments(&options.cast_hosts),
        options.cast_port,
        source_argument,
        recommendations.balanced_ms,
        options.fps,
        even(options.width),
        even(options.height),
        options.bitrate,
        tuning_arguments,
    );
    println!(
        "This estimates a reliable receiver buffer from sender-to-receiver telemetry; it is not a glass-to-glass display measurement."
    );
    if sampled_for < Duration::from_secs(10) || pipeline.count < options.fps as usize * 5 {
        println!(
            "Warning: the profile was short; collect at least ten seconds under representative motion before relying on it."
        );
    }
    Ok(())
}

fn source_command_argument(extend: bool, display_id: Option<u32>) -> String {
    if extend {
        " --extend".to_owned()
    } else if let Some(display_id) = display_id {
        format!(" --display {display_id}")
    } else {
        String::new()
    }
}

fn print_synthetic_breakdown(stats: &MirrorStats, samples: &[LatencySample]) -> Result<()> {
    println!("\nSynthetic phase breakdown:");
    for phase in SyntheticPhase::ALL {
        let phase_samples: Vec<_> = samples
            .iter()
            .filter(|sample| sample.synthetic_phase == Some(phase))
            .copied()
            .collect();
        let Some(pipeline) = LatencyDistribution::from_values(
            phase_samples
                .iter()
                .map(|sample| sample.pipeline_micros)
                .collect(),
        ) else {
            println!("  {:<14} no acknowledged frames", phase.label());
            continue;
        };
        let transport = LatencyDistribution::from_values(
            phase_samples
                .iter()
                .map(|sample| sample.transport_micros)
                .collect(),
        )
        .expect("synthetic pipeline and transport sample counts match");
        let encoded_total: u128 = phase_samples
            .iter()
            .map(|sample| u128::from(sample.encoded_bytes))
            .sum();
        let average_kib = encoded_total as f64 / phase_samples.len() as f64 / 1024.0;
        let maximum_kib = phase_samples
            .iter()
            .map(|sample| sample.encoded_bytes)
            .max()
            .unwrap_or(0) as f64
            / 1024.0;
        let packets = stats.synthetic_phase_packets[phase.index()].load(Ordering::Relaxed);
        let retransmissions =
            stats.synthetic_phase_retransmissions[phase.index()].load(Ordering::Relaxed);
        let retransmission_rate = if packets == 0 {
            0.0
        } else {
            retransmissions as f64 * 100.0 / packets as f64
        };
        println!(
            "  {:<14} {:>4} frames | frame avg {:>6.1} KiB, max {:>6.1} KiB | ready→ACK p95 {:>6.1} ms, p99 {:>6.1} ms | network p95 {:>6.1} ms | retrans {:>5.2}%",
            phase.label(),
            pipeline.count,
            average_kib,
            maximum_kib,
            micros_to_millis(pipeline.p95_micros),
            micros_to_millis(pipeline.p99_micros),
            micros_to_millis(transport.p95_micros),
            retransmission_rate
        );
    }

    let render_samples = stats
        .synthetic_render_micros
        .lock()
        .map_err(|_| anyhow!("synthetic render timing lock was poisoned"))?
        .clone();
    if let Some(render) = LatencyDistribution::from_values(render_samples) {
        println!(
            "  Generator: {} frames, render p95 {:.1} ms, max {:.1} ms, {} skipped schedule slots",
            stats.synthetic_generated_frames.load(Ordering::Relaxed),
            micros_to_millis(render.p95_micros),
            micros_to_millis(render.max_micros),
            stats.synthetic_skipped_frames.load(Ordering::Relaxed)
        );
    }
    println!(
        "  Synthetic drawing time is reported separately and excluded from frame-ready→ACK latency."
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LatencyRecommendations {
    aggressive_ms: u64,
    balanced_ms: u64,
    resilient_ms: u64,
}

fn recommend_latency(
    pipeline: LatencyDistribution,
    fps: u32,
    retransmissions: u64,
    packets: u64,
) -> LatencyRecommendations {
    let frame_ms = 1_000_u64.div_ceil(u64::from(fps.max(1)));
    let loss_basis_points = retransmissions
        .saturating_mul(10_000)
        .checked_div(packets)
        .unwrap_or(0);
    let loss_margin_ms = match loss_basis_points {
        100.. => 100,
        10..=99 => 50,
        1..=9 => 25,
        _ => 0,
    };
    let p95_ms = micros_to_millis_ceil(pipeline.p95_micros);
    let p99_ms = micros_to_millis_ceil(pipeline.p99_micros);
    let max_ms = micros_to_millis_ceil(pipeline.max_micros);
    let aggressive_ms = round_target_delay(p95_ms.saturating_add((frame_ms / 2).max(10)));
    let balanced_ms = round_target_delay(
        p99_ms
            .saturating_add(frame_ms)
            .saturating_add(loss_margin_ms),
    )
    .max(aggressive_ms);
    let resilient_ms = round_target_delay(
        max_ms
            .max(p99_ms.saturating_add(frame_ms))
            .saturating_add(frame_ms)
            .saturating_add(loss_margin_ms),
    )
    .max(balanced_ms);
    LatencyRecommendations {
        aggressive_ms,
        balanced_ms,
        resilient_ms,
    }
}

fn round_target_delay(milliseconds: u64) -> u64 {
    const STEPS: [u64; 23] = [
        10, 15, 20, 25, 33, 40, 50, 60, 75, 100, 125, 150, 200, 250, 300, 400, 500, 750, 1_000,
        1_500, 2_000, 3_000, 5_000,
    ];
    STEPS
        .into_iter()
        .find(|step| *step >= milliseconds)
        .unwrap_or(5_000)
}

fn retransmission_percent(stats: &MirrorStats) -> f64 {
    let packets = stats.rtp_packets.load(Ordering::Relaxed);
    if packets == 0 {
        return 0.0;
    }
    stats.retransmissions.load(Ordering::Relaxed) as f64 * 100.0 / packets as f64
}

fn micros_to_millis(micros: u64) -> f64 {
    micros as f64 / 1_000.0
}

fn micros_to_millis_ceil(micros: u64) -> u64 {
    micros.div_ceil(1_000)
}

fn elapsed_micros(started: Instant) -> u64 {
    duration_micros(started.elapsed())
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn resolve_max_frame_age(configured: Option<Duration>, fps: u32) -> Option<Duration> {
    match configured {
        Some(duration) if duration.is_zero() => None,
        Some(duration) => Some(duration),
        None => Some(frame_offset(2, fps)),
    }
}

fn frame_pacing(packet_count: usize, keyframe: bool, fps: u32) -> Option<(usize, Duration)> {
    let packets_per_burst = if keyframe {
        KEYFRAME_PACKETS_PER_BURST
    } else {
        DELTA_PACKETS_PER_BURST
    };
    let breaks = packet_count.saturating_sub(1) / packets_per_burst;
    if breaks == 0 {
        return None;
    }
    let frame_quarter = frame_offset(1, fps).div_f64(4.0);
    let window = MAX_FRAME_PACING_WINDOW.min(frame_quarter);
    let interval = window / u32::try_from(breaks).unwrap_or(u32::MAX).max(1);
    (!interval.is_zero()).then_some((packets_per_burst, interval))
}

#[allow(clippy::too_many_arguments)]
fn rate_window_is_congested(
    current_bitrate: u64,
    acknowledged_bps: u64,
    nacks: u64,
    maximum_latency_micros: u64,
    latency_limit: Duration,
    in_flight_frames: u64,
    frame_backlog_limit: u64,
    in_flight_bytes: u64,
    byte_backlog_limit: u64,
) -> (bool, bool) {
    let target_is_binding = acknowledged_bps >= current_bitrate.saturating_div(2);
    let loss_or_latency = nacks >= 3 || maximum_latency_micros > duration_micros(latency_limit);
    let backlog = in_flight_frames > frame_backlog_limit || in_flight_bytes > byte_backlog_limit;
    (
        backlog || (target_is_binding && loss_or_latency),
        target_is_binding,
    )
}

#[derive(Clone, Copy)]
struct H264Level {
    name: &'static str,
    codec_parameter: &'static str,
    profile_level: ProfileLevel,
}

impl H264Level {
    fn for_stream(width: u32, height: u32, fps: u32, bitrate: u64) -> Result<Self> {
        let macroblocks_per_frame =
            u64::from(width.div_ceil(16)).saturating_mul(u64::from(height.div_ceil(16)));
        let macroblocks_per_second = macroblocks_per_frame.saturating_mul(u64::from(fps));
        let levels = [
            (
                3_600,
                108_000,
                14_000_000,
                Self {
                    name: "3.1",
                    codec_parameter: "avc1.42001F",
                    profile_level: ProfileLevel::H264Baseline3_1,
                },
            ),
            (
                5_120,
                216_000,
                20_000_000,
                Self {
                    name: "3.2",
                    codec_parameter: "avc1.420020",
                    profile_level: ProfileLevel::H264Baseline3_2,
                },
            ),
            (
                8_192,
                245_760,
                20_000_000,
                Self {
                    name: "4.0",
                    codec_parameter: "avc1.420028",
                    profile_level: ProfileLevel::H264Baseline4_0,
                },
            ),
            (
                8_192,
                245_760,
                50_000_000,
                Self {
                    name: "4.1",
                    codec_parameter: "avc1.420029",
                    profile_level: ProfileLevel::H264Baseline4_1,
                },
            ),
            (
                8_704,
                522_240,
                50_000_000,
                Self {
                    name: "4.2",
                    codec_parameter: "avc1.42002A",
                    profile_level: ProfileLevel::H264Baseline4_2,
                },
            ),
            (
                22_080,
                589_824,
                135_000_000,
                Self {
                    name: "5.0",
                    codec_parameter: "avc1.420032",
                    profile_level: ProfileLevel::H264Baseline5_0,
                },
            ),
            (
                36_864,
                983_040,
                240_000_000,
                Self {
                    name: "5.1",
                    codec_parameter: "avc1.420033",
                    profile_level: ProfileLevel::H264Baseline5_1,
                },
            ),
            (
                36_864,
                2_073_600,
                240_000_000,
                Self {
                    name: "5.2",
                    codec_parameter: "avc1.420034",
                    profile_level: ProfileLevel::H264Baseline5_2,
                },
            ),
        ];
        levels
            .into_iter()
            .find(|(max_frame, max_second, max_bitrate, _)| {
                macroblocks_per_frame <= *max_frame
                    && macroblocks_per_second <= *max_second
                    && bitrate <= *max_bitrate
            })
            .map(|(_, _, _, level)| level)
            .ok_or_else(|| {
                anyhow!(
                    "{}x{} at {} fps and {:.2} Mbit/s exceeds H.264 Baseline level 5.2",
                    width,
                    height,
                    fps,
                    bitrate as f64 / 1_000_000.0
                )
            })
    }
}

fn configure_low_latency_encoder(
    encoder: &CompressionSession,
    prioritize_speed: bool,
    stats: &MirrorStats,
) {
    match set_encoder_i32(
        encoder,
        unsafe { videotoolbox::ffi::kVTCompressionPropertyKey_MaxFrameDelayCount },
        0,
    ) {
        Ok(()) => {
            stats
                .vt_max_frame_delay_applied
                .store(true, Ordering::Relaxed);
            log::debug!("VideoToolbox MaxFrameDelayCount=0 applied");
        }
        Err(error) => log::debug!(
            "VideoToolbox does not expose MaxFrameDelayCount=0 on this encoder path ({error:#}); per-frame CompleteFrames still makes output synchronous"
        ),
    }
    match set_encoder_bool(
        encoder,
        unsafe { videotoolbox::ffi::kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality },
        prioritize_speed,
    ) {
        Ok(()) => {
            stats
                .vt_speed_priority_applied
                .store(prioritize_speed, Ordering::Relaxed);
            log::debug!(
                "VideoToolbox PrioritizeEncodingSpeedOverQuality={prioritize_speed} applied"
            );
        }
        Err(error) => log::warn!(
            "VideoToolbox rejected PrioritizeEncodingSpeedOverQuality={prioritize_speed}: {error:#}"
        ),
    }
}

fn set_encoder_i32(
    encoder: &CompressionSession,
    key: videotoolbox::ffi::CFStringRef,
    value: i32,
) -> Result<()> {
    let number = unsafe {
        videotoolbox::ffi::CFNumberCreate(
            videotoolbox::ffi::kCFAllocatorDefault,
            videotoolbox::ffi::kCFNumberSInt32Type,
            std::ptr::from_ref(&value).cast(),
        )
    };
    if number.is_null() {
        bail!("CoreFoundation could not allocate a VideoToolbox property number");
    }
    let result = unsafe { encoder.set_property(key, number.cast()) }
        .context("could not set an integer VideoToolbox property");
    unsafe { videotoolbox::ffi::CFRelease(number.cast()) };
    result
}

fn set_encoder_bool(
    encoder: &CompressionSession,
    key: videotoolbox::ffi::CFStringRef,
    value: bool,
) -> Result<()> {
    let value = if value {
        unsafe { videotoolbox::ffi::kCFBooleanTrue }
    } else {
        unsafe { videotoolbox::ffi::kCFBooleanFalse }
    };
    unsafe { encoder.set_property(key, value.cast()) }
        .context("could not set a boolean VideoToolbox property")
}

#[derive(Clone)]
struct NegotiatedTransport {
    udp_port: u16,
    session_id: String,
    sender_ssrc: u32,
    receiver_ssrc: u32,
    aes_key: [u8; 16],
    aes_iv_mask: [u8; 16],
    audio: Option<NegotiatedAudioTransport>,
    receiver_frame_rate: Option<u32>,
    control: Arc<CastStreamingControlState>,
}

#[derive(Clone)]
struct NegotiatedAudioTransport {
    sender_ssrc: u32,
    receiver_ssrc: u32,
    aes_key: [u8; 16],
    aes_iv_mask: [u8; 16],
}

struct NegotiatedTarget {
    host: IpAddr,
    port: u16,
    transport: NegotiatedTransport,
    stopped: bool,
}

impl NegotiatedTarget {
    fn ensure_alive(&self) -> Result<()> {
        if let Some(error) = take_failure(&self.transport.control.failure)? {
            bail!(
                "Cast Streaming control connection to {} failed: {error}",
                self.host
            );
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        self.transport
            .control
            .close_expected
            .store(true, Ordering::SeqCst);
        stop_cast_streaming_session(self.host, self.port, &self.transport.session_id)
            .with_context(|| format!("could not stop Cast Streaming receiver {}", self.host))
    }
}

impl Drop for NegotiatedTarget {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            log::warn!("could not stop Cast Streaming receiver cleanly: {error:#}");
        }
    }
}

struct CastStreamingControlState {
    close_expected: AtomicBool,
    ready: AtomicBool,
    failure: Mutex<Option<String>>,
    volume_commands: mpsc::Sender<ReceiverVolumeCommand>,
}

impl CastStreamingControlState {
    fn request_volume(&self, command: ReceiverVolumeCommand) {
        if self.volume_commands.send(command).is_err() {
            log::warn!("Cast Streaming volume control channel has closed");
        }
    }
}

#[cfg(test)]
impl Default for CastStreamingControlState {
    fn default() -> Self {
        let (volume_commands, _) = mpsc::channel();
        Self {
            close_expected: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            failure: Mutex::new(None),
            volume_commands,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ReceiverVolumeCommand {
    SetLevel(f32),
    ToggleMute,
}

#[allow(clippy::too_many_arguments)]
fn negotiate_cast_streaming_group(
    hosts: &[IpAddr],
    port: u16,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    target_delay: Duration,
    h264_level: H264Level,
    audio: bool,
) -> Result<Vec<NegotiatedTarget>> {
    let (result_sender, result_receiver) = mpsc::channel();
    thread::scope(|scope| {
        for (index, host) in hosts.iter().copied().enumerate() {
            let result_sender = result_sender.clone();
            scope.spawn(move || {
                let result = negotiate_cast_streaming(
                    host,
                    port,
                    width,
                    height,
                    fps,
                    bitrate,
                    target_delay,
                    h264_level,
                    audio,
                );
                let _ = result_sender.send((index, host, result));
            });
        }
        drop(result_sender);
    });

    let mut slots: Vec<Option<(IpAddr, Result<NegotiatedTransport>)>> =
        (0..hosts.len()).map(|_| None).collect();
    for (index, host, result) in result_receiver {
        slots[index] = Some((host, result));
    }

    let mut targets = Vec::with_capacity(hosts.len());
    let mut errors = Vec::new();
    for (index, slot) in slots.into_iter().enumerate() {
        match slot {
            Some((host, Ok(transport))) => targets.push(NegotiatedTarget {
                host,
                port,
                transport,
                stopped: false,
            }),
            Some((host, Err(error))) => errors.push(format!("{host}: {error:#}")),
            None => errors.push(format!("{}: negotiation worker stopped", hosts[index])),
        }
    }
    if !errors.is_empty() {
        for target in &mut targets {
            if let Err(error) = target.stop() {
                log::warn!("cleanup after partial group startup failed: {error:#}");
            }
        }
        bail!("could not start every Cast receiver: {}", errors.join("; "));
    }
    Ok(targets)
}

fn negotiated_group_frame_rate(targets: &[NegotiatedTarget], requested_fps: u32) -> u32 {
    targets
        .iter()
        .filter_map(|target| target.transport.receiver_frame_rate)
        .filter(|fps| *fps > 0)
        .fold(requested_fps, u32::min)
        .max(1)
}

#[allow(clippy::too_many_arguments)]
fn negotiate_cast_streaming(
    host: IpAddr,
    port: u16,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    target_delay: Duration,
    h264_level: H264Level,
    audio: bool,
) -> Result<NegotiatedTransport> {
    let material = OfferMaterial::random(audio)?;
    let offer = CastStreamingOfferMessage::new(
        &material,
        width,
        height,
        fps,
        bitrate,
        target_delay,
        h264_level,
    );
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let (volume_commands, volume_command_receiver) = mpsc::channel();
    let thread_control = Arc::new(CastStreamingControlState {
        close_expected: AtomicBool::new(false),
        ready: AtomicBool::new(false),
        failure: Mutex::new(None),
        volume_commands,
    });
    thread::Builder::new()
        .name("cast-mirror-control".into())
        .spawn(move || {
            let result = negotiate_cast_streaming_inner(
                host,
                port,
                material,
                offer,
                &ready_sender,
                Arc::clone(&thread_control),
                volume_command_receiver,
            );
            if let Err(error) = result {
                let message = format!("{error:#}");
                if thread_control.ready.load(Ordering::SeqCst) {
                    if thread_control.close_expected.load(Ordering::SeqCst) {
                        log::debug!(
                            "Cast Streaming control connection ended after planned teardown: {message}"
                        );
                    } else {
                        store_source_failure(&thread_control.failure, message.clone());
                        log::warn!("Cast Streaming control connection ended: {message}");
                    }
                } else if ready_sender.send(Err(error)).is_err() {
                    log::warn!(
                        "Cast Streaming negotiation ended after the caller stopped waiting: {message}"
                    );
                }
            }
        })
        .context("could not start Cast Streaming control thread")?;

    ready_receiver
        .recv_timeout(Duration::from_secs(20))
        .context("Cast mirroring receiver did not answer the stream offer within 20 seconds")?
}

fn negotiate_cast_streaming_inner(
    host: IpAddr,
    port: u16,
    material: OfferMaterial,
    offer: CastStreamingOfferMessage,
    ready: &mpsc::SyncSender<Result<NegotiatedTransport>>,
    shared_control: Arc<CastStreamingControlState>,
    volume_commands: mpsc::Receiver<ReceiverVolumeCommand>,
) -> Result<()> {
    eprintln!("Connecting to Cast receiver at {host}:{port}...");
    let device = CastDevice::connect_without_host_verification(host.to_string(), port)
        .with_context(|| format!("could not connect to Cast device at {host}:{port}"))?;
    device
        .connection
        .connect("receiver-0")
        .context("could not initialize the Cast receiver channel")?;
    device
        .heartbeat
        .ping()
        .context("could not initialize the Cast heartbeat")?;

    let status = device
        .receiver
        .get_status()
        .context("could not inspect the Cast receiver before mirroring")?;
    let receiver_muted = status.volume.muted.unwrap_or(false);
    for existing in status
        .applications
        .into_iter()
        .filter(|application| application.app_id == CAST_STREAMING_APP_ID)
    {
        log::debug!(
            "stopping stale Cast Streaming session {} before relaunch",
            existing.session_id
        );
        device
            .receiver
            .stop_app(existing.session_id)
            .context("could not stop the previous Cast Streaming session")?;
    }

    eprintln!("Launching the built-in Chrome Mirroring receiver...");
    let application = device
        .receiver
        .launch_app(&CastDeviceApp::Custom(CAST_STREAMING_APP_ID.into()))
        .context("could not launch the built-in Cast Streaming receiver")?;
    log::debug!(
        "launched Cast Streaming app {} ({}) with transport {} and namespaces {:?}",
        application.app_id,
        application.display_name,
        application.transport_id,
        application.namespaces
    );
    device
        .connection
        .connect(&application.transport_id)
        .context("could not connect to the Cast Streaming application")?;
    device
        .receiver
        .send_message(&application.transport_id, CAST_STREAMING_NAMESPACE, &offer)
        .context("could not send the Cast Streaming offer")?;
    log::debug!("sent Cast Streaming offer: {offer:?}");

    let answer = wait_for_answer(&device, offer.sequence_number)?;
    let video_index = if material.audio.is_some() { 1 } else { 0 };
    let selected = answer
        .send_indexes
        .iter()
        .position(|index| *index == video_index)
        .ok_or_else(|| anyhow!("Cast receiver did not select the offered H.264 video stream"))?;
    let receiver_ssrc = *answer
        .ssrcs
        .get(selected)
        .ok_or_else(|| anyhow!("Cast receiver answer omitted the video RTCP SSRC"))?;
    let udp_port =
        u16::try_from(answer.udp_port).context("Cast receiver returned an invalid UDP port")?;
    let receiver_frame_rate = answer.display.as_ref().and_then(answer_display_frame_rate);
    let audio = material.audio.as_ref().and_then(|audio| {
        let selected = answer.send_indexes.iter().position(|index| *index == 0)?;
        let receiver_ssrc = *answer.ssrcs.get(selected)?;
        Some(NegotiatedAudioTransport {
            sender_ssrc: audio.sender_ssrc,
            receiver_ssrc,
            aes_key: audio.aes_key,
            aes_iv_mask: audio.aes_iv_mask,
        })
    });
    if material.audio.is_some() && audio.is_none() {
        eprintln!(
            "Receiver {host} did not accept desktop audio; continuing with video only for that receiver."
        );
    }
    log::debug!(
        "Cast Streaming answer selected video index {video_index}, audio={}, UDP port {udp_port}, receiver SSRC {receiver_ssrc}, constraints={:?}, display={:?}",
        audio.is_some(),
        answer.constraints,
        answer.display
    );

    ready
        .send(Ok(NegotiatedTransport {
            udp_port,
            session_id: application.session_id.clone(),
            sender_ssrc: material.sender_ssrc,
            receiver_ssrc,
            aes_key: material.aes_key,
            aes_iv_mask: material.aes_iv_mask,
            audio,
            receiver_frame_rate,
            control: Arc::clone(&shared_control),
        }))
        .map_err(|_| anyhow!("caller stopped waiting for Cast Streaming negotiation"))?;
    shared_control.ready.store(true, Ordering::SeqCst);

    device
        .set_read_timeout(Some(Duration::from_millis(100)))
        .context("could not make Cast Streaming controls interruptible")?;
    monitor_cast_streaming_control(&device, &volume_commands, receiver_muted)
}

fn stop_cast_streaming_session(host: IpAddr, port: u16, session_id: &str) -> Result<()> {
    log::debug!("stopping Cast Streaming session {session_id}");
    let device = CastDevice::connect_without_host_verification(host.to_string(), port)
        .with_context(|| format!("could not reconnect to Cast device at {host}:{port}"))?;
    device
        .connection
        .connect("receiver-0")
        .context("could not initialize the Cast receiver teardown channel")?;
    device
        .receiver
        .stop_app(session_id.to_owned())
        .context("Cast receiver rejected the mirroring session stop")
}

fn wait_for_answer(device: &CastDevice<'_>, expected_sequence: u32) -> Result<AnswerData> {
    loop {
        match receive_cast_streaming_message(device)
            .context("could not receive Cast Streaming negotiation message")?
        {
            ChannelMessage::Heartbeat(HeartbeatResponse::Ping) => {
                device
                    .heartbeat
                    .pong()
                    .context("could not answer Cast heartbeat")?;
            }
            ChannelMessage::Connection(ConnectionResponse::Close) => {
                bail!("Cast Streaming receiver closed during negotiation");
            }
            ChannelMessage::Raw(message) if message.namespace == CAST_STREAMING_NAMESPACE => {
                let Some(answer) = parse_answer(message, expected_sequence)? else {
                    continue;
                };
                return Ok(answer);
            }
            message => log::trace!("Cast Streaming negotiation message: {message:?}"),
        }
    }
}

fn answer_display_frame_rate(display: &Value) -> Option<u32> {
    let value = display.get("dimensions")?.get("frameRate")?;
    match value {
        Value::Number(number) => number.as_u64()?.try_into().ok(),
        Value::String(value) => {
            let (numerator, denominator) = value
                .split_once('/')
                .map_or((value.as_str(), "1"), |parts| parts);
            let numerator = numerator.parse::<u32>().ok()?;
            let denominator = denominator.parse::<u32>().ok()?.max(1);
            Some(numerator.div_ceil(denominator))
        }
        _ => None,
    }
}

fn parse_answer(message: CastMessage, expected_sequence: u32) -> Result<Option<AnswerData>> {
    let CastMessagePayload::String(payload) = message.payload else {
        bail!("Cast Streaming receiver sent a binary negotiation message");
    };
    let envelope: AnswerMessage = serde_json::from_str(&payload)
        .context("Cast Streaming receiver sent malformed answer JSON")?;
    if envelope.message_type != "ANSWER" {
        log::debug!("Cast Streaming receiver message: {payload}");
        return Ok(None);
    }
    if envelope.sequence_number != expected_sequence {
        log::debug!(
            "ignoring Cast Streaming answer for sequence {} while waiting for {expected_sequence}",
            envelope.sequence_number
        );
        return Ok(None);
    }
    if envelope.result != "ok" {
        bail!(
            "Cast Streaming receiver rejected the offer: {}",
            envelope.error.unwrap_or(Value::Null)
        );
    }
    Ok(Some(envelope.answer.ok_or_else(|| {
        anyhow!("successful Cast Streaming answer had no answer body")
    })?))
}

fn receive_cast_streaming_message(
    device: &CastDevice<'_>,
) -> std::result::Result<ChannelMessage, CastError> {
    if let Some(message) = device.receive_buffered()? {
        return Ok(message);
    }
    let message = device.receive_raw()?;
    if device.connection.can_handle(&message) {
        return Ok(ChannelMessage::Connection(
            device.connection.parse(&message)?,
        ));
    }
    if device.heartbeat.can_handle(&message) {
        return Ok(ChannelMessage::Heartbeat(device.heartbeat.parse(&message)?));
    }
    Ok(ChannelMessage::Raw(message))
}

fn monitor_cast_streaming_control(
    device: &CastDevice<'_>,
    volume_commands: &mpsc::Receiver<ReceiverVolumeCommand>,
    mut muted: bool,
) -> Result<()> {
    log::debug!("monitoring Cast Streaming control channel");
    loop {
        while let Ok(command) = volume_commands.try_recv() {
            let result = match command {
                ReceiverVolumeCommand::SetLevel(level) => {
                    device.receiver.set_volume(level.clamp(0.0, 1.0))
                }
                ReceiverVolumeCommand::ToggleMute => device.receiver.set_volume(!muted),
            };
            match result {
                Ok(volume) => muted = volume.muted.unwrap_or(muted),
                Err(error) => log::warn!("receiver rejected desktop volume control: {error}"),
            }
        }
        match receive_cast_streaming_message(device) {
            Ok(ChannelMessage::Heartbeat(HeartbeatResponse::Ping)) => {
                device
                    .heartbeat
                    .pong()
                    .context("could not answer Cast heartbeat")?;
            }
            Ok(ChannelMessage::Connection(ConnectionResponse::Close)) => {
                bail!("Cast Streaming receiver closed the application connection");
            }
            Ok(ChannelMessage::Raw(message)) => {
                log::debug!("Cast Streaming receiver message: {message:?}");
            }
            Ok(message) => log::trace!("Cast Streaming control message: {message:?}"),
            Err(error) if cast_receive_timed_out(&error) => {}
            Err(error) => {
                return Err(error)
                    .context("could not receive the next Cast Streaming control message");
            }
        }
    }
}

fn cast_receive_timed_out(error: &CastError) -> bool {
    matches!(
        error,
        CastError::Io(error)
            if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
    )
}

#[derive(Debug)]
struct OfferMaterial {
    sequence_number: u32,
    sender_ssrc: u32,
    aes_key: [u8; 16],
    aes_iv_mask: [u8; 16],
    audio: Option<StreamMaterial>,
}

#[derive(Debug)]
struct StreamMaterial {
    sender_ssrc: u32,
    aes_key: [u8; 16],
    aes_iv_mask: [u8; 16],
}

impl OfferMaterial {
    fn random(with_audio: bool) -> Result<Self> {
        let mut random = [0_u8; 76];
        getrandom::fill(&mut random).context("could not generate Cast Streaming session keys")?;
        let sequence_number = u32::from_be_bytes(random[0..4].try_into().unwrap()) & 0x7fff_ffff;
        let sender_ssrc = 50_001 + u32::from_be_bytes(random[4..8].try_into().unwrap()) % 50_000;
        Ok(Self {
            sequence_number: sequence_number.max(1),
            sender_ssrc,
            aes_key: random[8..24].try_into().unwrap(),
            aes_iv_mask: random[24..40].try_into().unwrap(),
            audio: with_audio.then(|| StreamMaterial {
                sender_ssrc: 100_001
                    + u32::from_be_bytes(random[40..44].try_into().unwrap()) % 50_000,
                aes_key: random[44..60].try_into().unwrap(),
                aes_iv_mask: random[60..76].try_into().unwrap(),
            }),
        })
    }
}

#[derive(Debug, Serialize)]
struct CastStreamingOfferMessage {
    #[serde(rename = "type")]
    message_type: &'static str,
    #[serde(rename = "seqNum")]
    sequence_number: u32,
    offer: CastStreamingOffer,
}

impl CastStreamingOfferMessage {
    fn new(
        material: &OfferMaterial,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        target_delay: Duration,
        h264_level: H264Level,
    ) -> Self {
        Self {
            message_type: "OFFER",
            sequence_number: material.sequence_number,
            offer: CastStreamingOffer {
                cast_mode: "mirroring",
                supported_streams: {
                    let mut streams =
                        Vec::with_capacity(if material.audio.is_some() { 2 } else { 1 });
                    if let Some(audio_material) = &material.audio {
                        streams.push(StreamOffer::Audio(AudioStreamOffer {
                            index: 0,
                            stream_type: "audio_source",
                            channels: audio::CHANNELS as u8,
                            codec_name: "aac",
                            codec_parameter: "mp4a.40.2",
                            rtp_profile: "cast",
                            rtp_payload_type: audio::RTP_PAYLOAD_TYPE,
                            ssrc: audio_material.sender_ssrc,
                            target_delay: target_delay.as_millis() as u64,
                            aes_key: hex(&audio_material.aes_key),
                            aes_iv_mask: hex(&audio_material.aes_iv_mask),
                            receiver_rtcp_event_log: false,
                            time_base: "1/48000",
                            bit_rate: audio::BITRATE,
                        }));
                    }
                    streams.push(StreamOffer::Video(VideoStreamOffer {
                        index: if material.audio.is_some() { 1 } else { 0 },
                        stream_type: "video_source",
                        channels: 1,
                        codec_name: "h264",
                        codec_parameter: h264_level.codec_parameter,
                        rtp_profile: "cast",
                        rtp_payload_type: RTP_H264_PAYLOAD_TYPE,
                        ssrc: material.sender_ssrc,
                        target_delay: target_delay.as_millis() as u64,
                        aes_key: hex(&material.aes_key),
                        aes_iv_mask: hex(&material.aes_iv_mask),
                        receiver_rtcp_event_log: false,
                        time_base: "1/90000",
                        max_frame_rate: format!("{fps}/1"),
                        max_bit_rate: bitrate,
                        profile: "baseline",
                        level: h264_level.name,
                        error_recovery_mode: "castv2",
                        resolutions: vec![VideoResolution { width, height }],
                    }));
                    streams
                },
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct CastStreamingOffer {
    #[serde(rename = "castMode")]
    cast_mode: &'static str,
    #[serde(rename = "supportedStreams")]
    supported_streams: Vec<StreamOffer>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum StreamOffer {
    Audio(AudioStreamOffer),
    Video(VideoStreamOffer),
}

#[derive(Debug, Serialize)]
struct AudioStreamOffer {
    index: u8,
    #[serde(rename = "type")]
    stream_type: &'static str,
    channels: u8,
    #[serde(rename = "codecName")]
    codec_name: &'static str,
    #[serde(rename = "codecParameter")]
    codec_parameter: &'static str,
    #[serde(rename = "rtpProfile")]
    rtp_profile: &'static str,
    #[serde(rename = "rtpPayloadType")]
    rtp_payload_type: u8,
    ssrc: u32,
    #[serde(rename = "targetDelay")]
    target_delay: u64,
    #[serde(rename = "aesKey")]
    aes_key: String,
    #[serde(rename = "aesIvMask")]
    aes_iv_mask: String,
    #[serde(rename = "receiverRtcpEventLog")]
    receiver_rtcp_event_log: bool,
    #[serde(rename = "timeBase")]
    time_base: &'static str,
    #[serde(rename = "bitRate")]
    bit_rate: u32,
}

#[derive(Debug, Serialize)]
struct VideoStreamOffer {
    index: u8,
    #[serde(rename = "type")]
    stream_type: &'static str,
    channels: u8,
    #[serde(rename = "codecName")]
    codec_name: &'static str,
    #[serde(rename = "codecParameter")]
    codec_parameter: &'static str,
    #[serde(rename = "rtpProfile")]
    rtp_profile: &'static str,
    #[serde(rename = "rtpPayloadType")]
    rtp_payload_type: u8,
    ssrc: u32,
    #[serde(rename = "targetDelay")]
    target_delay: u64,
    #[serde(rename = "aesKey")]
    aes_key: String,
    #[serde(rename = "aesIvMask")]
    aes_iv_mask: String,
    #[serde(rename = "receiverRtcpEventLog")]
    receiver_rtcp_event_log: bool,
    #[serde(rename = "timeBase")]
    time_base: &'static str,
    #[serde(rename = "maxFrameRate")]
    max_frame_rate: String,
    #[serde(rename = "maxBitRate")]
    max_bit_rate: u32,
    profile: &'static str,
    level: &'static str,
    #[serde(rename = "errorRecoveryMode")]
    error_recovery_mode: &'static str,
    resolutions: Vec<VideoResolution>,
}

#[derive(Debug, Serialize)]
struct VideoResolution {
    width: u32,
    height: u32,
}

#[derive(Debug, Deserialize)]
struct AnswerMessage {
    #[serde(rename = "type")]
    message_type: String,
    #[serde(rename = "seqNum")]
    sequence_number: u32,
    result: String,
    answer: Option<AnswerData>,
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct AnswerData {
    #[serde(rename = "udpPort")]
    udp_port: u32,
    #[serde(rename = "sendIndexes")]
    send_indexes: Vec<u32>,
    ssrcs: Vec<u32>,
    constraints: Option<Value>,
    display: Option<Value>,
}

#[derive(Default)]
struct MirrorStats {
    requested_frame_rate: AtomicU64,
    effective_frame_rate: AtomicU64,
    raw_frames_submitted: AtomicU64,
    raw_frames_replaced: AtomicU64,
    raw_frames_expired: AtomicU64,
    frames: AtomicU64,
    keyframes: AtomicU64,
    encoded_bytes: AtomicU64,
    max_frame_bytes: AtomicU64,
    rtp_packets: AtomicU64,
    retransmissions: AtomicU64,
    feedback_packets: AtomicU64,
    nacks: AtomicU64,
    checkpoint_samples: AtomicU64,
    checkpoint_latency_micros: AtomicU64,
    max_checkpoint_latency_micros: AtomicU64,
    receiver_playout_delay_ms: AtomicU64,
    current_in_flight_frames: AtomicU64,
    max_in_flight_frames: AtomicU64,
    current_in_flight_bytes: AtomicU64,
    max_in_flight_bytes: AtomicU64,
    history_evictions: AtomicU64,
    pacing_sleep_micros: AtomicU64,
    max_pacing_sleep_micros: AtomicU64,
    adaptive_bitrate_increases: AtomicU64,
    adaptive_bitrate_decreases: AtomicU64,
    adaptive_bitrate_apply_failures: AtomicU64,
    current_target_bitrate: AtomicU64,
    minimum_target_bitrate: AtomicU64,
    vt_max_frame_delay_applied: AtomicBool,
    vt_speed_priority_applied: AtomicBool,
    latency_samples: Mutex<Vec<LatencySample>>,
    synthetic_render_micros: Mutex<Vec<u64>>,
    synthetic_generated_frames: AtomicU64,
    synthetic_skipped_frames: AtomicU64,
    synthetic_phase_packets: [AtomicU64; 4],
    synthetic_phase_retransmissions: [AtomicU64; 4],
}

fn merge_capture_stats(source: &MirrorStats, target: &MirrorStats) -> Result<()> {
    macro_rules! copy_u64 {
        ($field:ident) => {
            target
                .$field
                .store(source.$field.load(Ordering::Relaxed), Ordering::Relaxed)
        };
    }
    copy_u64!(requested_frame_rate);
    copy_u64!(effective_frame_rate);
    copy_u64!(raw_frames_submitted);
    copy_u64!(raw_frames_replaced);
    copy_u64!(raw_frames_expired);
    copy_u64!(adaptive_bitrate_increases);
    copy_u64!(adaptive_bitrate_decreases);
    copy_u64!(adaptive_bitrate_apply_failures);
    copy_u64!(current_target_bitrate);
    copy_u64!(minimum_target_bitrate);
    copy_u64!(synthetic_generated_frames);
    copy_u64!(synthetic_skipped_frames);
    target.vt_max_frame_delay_applied.store(
        source.vt_max_frame_delay_applied.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    target.vt_speed_priority_applied.store(
        source.vt_speed_priority_applied.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    let render_samples = source
        .synthetic_render_micros
        .lock()
        .map_err(|_| anyhow!("synthetic render timing lock was poisoned"))?
        .clone();
    *target
        .synthetic_render_micros
        .lock()
        .map_err(|_| anyhow!("synthetic render timing lock was poisoned"))? = render_samples;
    Ok(())
}

struct AdaptiveRateControl {
    enabled: bool,
    minimum_bitrate: u64,
    maximum_bitrate: u64,
    target_bitrate: AtomicU64,
    stats: Arc<MirrorStats>,
    group: Mutex<GroupRateState>,
}

#[derive(Clone, Copy)]
struct RateWindowHealth {
    congested: bool,
    acknowledged_bps: u64,
}

struct GroupRateState {
    reports: Vec<Option<RateWindowHealth>>,
    healthy_rounds: u8,
}

impl AdaptiveRateControl {
    #[cfg(test)]
    fn new(maximum_bitrate: u64, enabled: bool, stats: Arc<MirrorStats>) -> Self {
        Self::new_group(maximum_bitrate, enabled, stats, 1)
    }

    fn new_group(
        maximum_bitrate: u64,
        enabled: bool,
        stats: Arc<MirrorStats>,
        participants: usize,
    ) -> Self {
        assert!(participants > 0, "rate-control group must not be empty");
        let minimum_bitrate = (maximum_bitrate / 4).max(500_000).min(maximum_bitrate);
        stats
            .current_target_bitrate
            .store(maximum_bitrate, Ordering::Relaxed);
        stats
            .minimum_target_bitrate
            .store(maximum_bitrate, Ordering::Relaxed);
        Self {
            enabled,
            minimum_bitrate,
            maximum_bitrate,
            target_bitrate: AtomicU64::new(maximum_bitrate),
            stats,
            group: Mutex::new(GroupRateState {
                reports: vec![None; participants],
                healthy_rounds: 0,
            }),
        }
    }

    fn target_bitrate(&self) -> u64 {
        self.target_bitrate.load(Ordering::Relaxed)
    }

    fn decrease(&self, _acknowledged_bps: u64) {
        if !self.enabled {
            return;
        }
        let current = self.target_bitrate();
        let next = current
            .saturating_mul(80)
            .saturating_div(100)
            .max(self.minimum_bitrate);
        self.set_target(next, false);
    }

    fn increase(&self) {
        if !self.enabled {
            return;
        }
        let current = self.target_bitrate();
        let next = current
            .saturating_mul(105)
            .saturating_div(100)
            .max(current.saturating_add(100_000))
            .min(self.maximum_bitrate);
        self.set_target(next, true);
    }

    fn report_window(&self, target: usize, health: RateWindowHealth) {
        let action = {
            let Ok(mut group) = self.group.lock() else {
                log::warn!("adaptive bitrate group lock was poisoned");
                return;
            };
            let Some(slot) = group.reports.get_mut(target) else {
                log::warn!("adaptive bitrate received an unknown target index {target}");
                return;
            };
            if let Some(existing) = slot.as_mut() {
                existing.congested |= health.congested;
                existing.acknowledged_bps = existing.acknowledged_bps.min(health.acknowledged_bps);
            } else {
                *slot = Some(health);
            }
            if group.reports.iter().any(Option::is_none) {
                return;
            }

            let congested = group
                .reports
                .iter()
                .flatten()
                .any(|report| report.congested);
            let acknowledged_bps = group
                .reports
                .iter()
                .flatten()
                .map(|report| report.acknowledged_bps)
                .min()
                .unwrap_or(0);
            group.reports.fill(None);
            if congested {
                group.healthy_rounds = 0;
                Some((false, acknowledged_bps))
            } else {
                group.healthy_rounds = group.healthy_rounds.saturating_add(1);
                if group.healthy_rounds >= RATE_CONTROL_HEALTHY_WINDOWS_BEFORE_INCREASE {
                    group.healthy_rounds = 0;
                    Some((true, acknowledged_bps))
                } else {
                    None
                }
            }
        };
        match action {
            Some((true, _)) => self.increase(),
            Some((false, acknowledged_bps)) => self.decrease(acknowledged_bps),
            None => {}
        }
    }

    fn set_target(&self, next: u64, increase: bool) {
        let current = self.target_bitrate();
        if next == current {
            return;
        }
        self.target_bitrate.store(next, Ordering::Relaxed);
        self.stats
            .current_target_bitrate
            .store(next, Ordering::Relaxed);
        self.stats
            .minimum_target_bitrate
            .fetch_min(next, Ordering::Relaxed);
        if increase {
            self.stats
                .adaptive_bitrate_increases
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.stats
                .adaptive_bitrate_decreases
                .fetch_add(1, Ordering::Relaxed);
        }
        log::debug!(
            "adaptive bitrate changed from {:.2} to {:.2} Mbit/s",
            current as f64 / 1_000_000.0,
            next as f64 / 1_000_000.0
        );
    }
}

#[derive(Clone, Copy, Debug)]
struct LatencySample {
    pipeline_micros: u64,
    transport_micros: u64,
    capture_age_micros: Option<u64>,
    queue_wait_micros: u64,
    encode_micros: u64,
    prepare_micros: u64,
    sender_lock_wait_micros: u64,
    send_micros: u64,
    in_flight_frames: u64,
    in_flight_bytes: u64,
    keyframe: bool,
    encoded_bytes: u64,
    synthetic_phase: Option<SyntheticPhase>,
}

#[derive(Clone, Copy)]
struct FrameTimings {
    pipeline_started_at: Instant,
    capture_age_micros: Option<u64>,
    queue_wait_micros: u64,
    encode_micros: u64,
    prepare_micros: u64,
    sender_lock_wait_micros: u64,
}

#[derive(Clone)]
struct StoredFrame {
    frame_id: u32,
    referenced_frame_id: u32,
    rtp_timestamp: u32,
    keyframe: bool,
    encrypted_data: Vec<u8>,
    pipeline_started_at: Instant,
    sent_at: Instant,
    capture_age_micros: Option<u64>,
    queue_wait_micros: u64,
    encode_micros: u64,
    prepare_micros: u64,
    sender_lock_wait_micros: u64,
    send_micros: u64,
    in_flight_frames: u64,
    in_flight_bytes: u64,
    synthetic_phase: Option<SyntheticPhase>,
}

struct CastRtpSender {
    socket: UdpSocket,
    max_packet_size: usize,
    sender_ssrc: u32,
    receiver_ssrc: u32,
    aes_key: [u8; 16],
    aes_iv_mask: [u8; 16],
    sequence_number: u16,
    next_frame_id: u32,
    history: VecDeque<StoredFrame>,
    packet_count: u32,
    octet_count: u32,
    last_sender_report: Option<Instant>,
    stats: Arc<MirrorStats>,
    rate_control: Arc<AdaptiveRateControl>,
    rate_control_target: usize,
    fps: u32,
    target_delay: Duration,
    payload_type: u8,
    rate_window_started: Instant,
    rate_window_acked_bytes: u64,
    rate_window_nacks: u64,
    rate_window_max_latency_micros: u64,
    rate_window_max_in_flight_frames: u64,
    rate_window_max_in_flight_bytes: u64,
}

impl CastRtpSender {
    #[allow(clippy::too_many_arguments)]
    fn new(
        socket: UdpSocket,
        ipv4: bool,
        sender_ssrc: u32,
        receiver_ssrc: u32,
        aes_key: [u8; 16],
        aes_iv_mask: [u8; 16],
        stats: Arc<MirrorStats>,
        rate_control: Arc<AdaptiveRateControl>,
        rate_control_target: usize,
        fps: u32,
        target_delay: Duration,
    ) -> Result<Self> {
        Self::new_with_payload(
            socket,
            ipv4,
            sender_ssrc,
            receiver_ssrc,
            aes_key,
            aes_iv_mask,
            stats,
            rate_control,
            rate_control_target,
            fps,
            target_delay,
            RTP_H264_PAYLOAD_TYPE,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_audio(
        socket: UdpSocket,
        ipv4: bool,
        sender_ssrc: u32,
        receiver_ssrc: u32,
        aes_key: [u8; 16],
        aes_iv_mask: [u8; 16],
        target_delay: Duration,
    ) -> Result<Self> {
        let stats = Arc::new(MirrorStats::default());
        let rate_control = Arc::new(AdaptiveRateControl::new_group(
            u64::from(audio::BITRATE),
            false,
            Arc::clone(&stats),
            1,
        ));
        Self::new_with_payload(
            socket,
            ipv4,
            sender_ssrc,
            receiver_ssrc,
            aes_key,
            aes_iv_mask,
            stats,
            rate_control,
            0,
            50,
            target_delay,
            audio::RTP_PAYLOAD_TYPE,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_payload(
        socket: UdpSocket,
        ipv4: bool,
        sender_ssrc: u32,
        receiver_ssrc: u32,
        aes_key: [u8; 16],
        aes_iv_mask: [u8; 16],
        stats: Arc<MirrorStats>,
        rate_control: Arc<AdaptiveRateControl>,
        rate_control_target: usize,
        fps: u32,
        target_delay: Duration,
        payload_type: u8,
    ) -> Result<Self> {
        let mut sequence = [0_u8; 2];
        getrandom::fill(&mut sequence).context("could not initialize RTP sequence number")?;
        Ok(Self {
            socket,
            max_packet_size: if ipv4 { 1472 } else { 1452 },
            sender_ssrc,
            receiver_ssrc,
            aes_key,
            aes_iv_mask,
            sequence_number: u16::from_be_bytes(sequence),
            next_frame_id: 0,
            history: VecDeque::with_capacity(MAX_UNACKED_FRAMES),
            packet_count: 0,
            octet_count: 0,
            last_sender_report: None,
            stats,
            rate_control,
            rate_control_target,
            fps,
            target_delay,
            payload_type,
            rate_window_started: Instant::now(),
            rate_window_acked_bytes: 0,
            rate_window_nacks: 0,
            rate_window_max_latency_micros: 0,
            rate_window_max_in_flight_frames: 0,
            rate_window_max_in_flight_bytes: 0,
        })
    }

    fn send_frame(
        &mut self,
        rtp_timestamp: u32,
        keyframe: bool,
        data: &[u8],
        timings: FrameTimings,
        synthetic_phase: Option<SyntheticPhase>,
    ) -> Result<()> {
        let send_started_at = Instant::now();
        let frame_id = self.next_frame_id;
        let referenced_frame_id = if keyframe {
            frame_id
        } else {
            frame_id.wrapping_sub(1)
        };
        let encrypted_data = encrypt_frame(data, frame_id, &self.aes_key, &self.aes_iv_mask);
        let in_flight_frames = self.history.len() as u64 + 1;
        let in_flight_bytes = self
            .history
            .iter()
            .map(|frame| frame.encrypted_data.len() as u64)
            .sum::<u64>()
            .saturating_add(encrypted_data.len() as u64);
        let mut frame = StoredFrame {
            frame_id,
            referenced_frame_id,
            rtp_timestamp,
            keyframe,
            encrypted_data,
            pipeline_started_at: timings.pipeline_started_at,
            sent_at: send_started_at,
            capture_age_micros: timings.capture_age_micros,
            queue_wait_micros: timings.queue_wait_micros,
            encode_micros: timings.encode_micros,
            prepare_micros: timings.prepare_micros,
            sender_lock_wait_micros: timings.sender_lock_wait_micros,
            send_micros: 0,
            in_flight_frames,
            in_flight_bytes,
            synthetic_phase,
        };

        if self
            .last_sender_report
            .is_none_or(|last| last.elapsed() >= RTCP_REPORT_INTERVAL)
        {
            self.send_sender_report(rtp_timestamp)?;
        }
        self.send_stored_frame(&frame, false, None)?;
        frame.send_micros = elapsed_micros(send_started_at);
        self.history.push_back(frame);
        while self.history.len() > MAX_UNACKED_FRAMES {
            self.history.pop_front();
            self.stats.history_evictions.fetch_add(1, Ordering::Relaxed);
        }
        self.update_in_flight_stats();
        self.next_frame_id = self.next_frame_id.wrapping_add(1);
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        self.stats
            .encoded_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.stats
            .max_frame_bytes
            .fetch_max(data.len() as u64, Ordering::Relaxed);
        if keyframe {
            self.stats.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn send_stored_frame(
        &mut self,
        frame: &StoredFrame,
        retransmission: bool,
        only_packet: Option<u16>,
    ) -> Result<()> {
        let max_payload = self
            .max_packet_size
            .checked_sub(23)
            .ok_or_else(|| anyhow!("RTP packet size is too small"))?;
        let packet_count = frame.encrypted_data.len().div_ceil(max_payload).max(1);
        if packet_count > u16::MAX as usize {
            bail!("encoded frame is too large for Cast RTP packet IDs");
        }
        let packet_ids: Vec<usize> = match only_packet {
            Some(packet_id) if packet_id == u16::MAX => (0..packet_count).collect(),
            Some(packet_id) if usize::from(packet_id) < packet_count => {
                vec![usize::from(packet_id)]
            }
            Some(packet_id) => {
                log::debug!(
                    "ignoring NACK for nonexistent packet {packet_id} of frame {} ({packet_count} packets)",
                    frame.frame_id
                );
                return Ok(());
            }
            None => (0..packet_count).collect(),
        };
        let pacing = (!retransmission && only_packet.is_none())
            .then(|| frame_pacing(packet_ids.len(), frame.keyframe, self.fps))
            .flatten();
        let mut pacing_sleep = Duration::ZERO;

        for (position, packet_id) in packet_ids.iter().copied().enumerate() {
            let packet = self.packetize(frame, packet_id, packet_count, max_payload);
            let sent = self
                .socket
                .send(&packet)
                .context("could not send Cast RTP packet")?;
            if sent != packet.len() {
                bail!("Cast RTP socket sent only {sent} of {} bytes", packet.len());
            }
            self.packet_count = self.packet_count.wrapping_add(1);
            self.octet_count = self
                .octet_count
                .wrapping_add(u32::try_from(packet.len().saturating_sub(19)).unwrap_or(u32::MAX));
            self.stats.rtp_packets.fetch_add(1, Ordering::Relaxed);
            if let Some(phase) = frame.synthetic_phase {
                self.stats.synthetic_phase_packets[phase.index()].fetch_add(1, Ordering::Relaxed);
            }
            if retransmission {
                self.stats.retransmissions.fetch_add(1, Ordering::Relaxed);
                if let Some(phase) = frame.synthetic_phase {
                    self.stats.synthetic_phase_retransmissions[phase.index()]
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            if let Some((packets_per_burst, interval)) = pacing
                && position + 1 < packet_ids.len()
                && (position + 1) % packets_per_burst == 0
            {
                thread::sleep(interval);
                pacing_sleep = pacing_sleep.saturating_add(interval);
            }
        }
        if !pacing_sleep.is_zero() {
            let paced_micros = duration_micros(pacing_sleep);
            self.stats
                .pacing_sleep_micros
                .fetch_add(paced_micros, Ordering::Relaxed);
            self.stats
                .max_pacing_sleep_micros
                .fetch_max(paced_micros, Ordering::Relaxed);
        }
        Ok(())
    }

    fn update_in_flight_stats(&self) {
        let frames = self.history.len() as u64;
        let bytes = self
            .history
            .iter()
            .map(|frame| frame.encrypted_data.len() as u64)
            .sum::<u64>();
        self.stats
            .current_in_flight_frames
            .store(frames, Ordering::Relaxed);
        self.stats
            .max_in_flight_frames
            .fetch_max(frames, Ordering::Relaxed);
        self.stats
            .current_in_flight_bytes
            .store(bytes, Ordering::Relaxed);
        self.stats
            .max_in_flight_bytes
            .fetch_max(bytes, Ordering::Relaxed);
    }

    fn packetize(
        &mut self,
        frame: &StoredFrame,
        packet_id: usize,
        packet_count: usize,
        max_payload: usize,
    ) -> Vec<u8> {
        let start = packet_id * max_payload;
        let end = (start + max_payload).min(frame.encrypted_data.len());
        let payload = &frame.encrypted_data[start..end];
        let mut packet = Vec::with_capacity(19 + payload.len());
        packet.push(0x80);
        packet.push(
            if packet_id + 1 == packet_count {
                0x80
            } else {
                0
            } | self.payload_type,
        );
        packet.extend_from_slice(&self.sequence_number.to_be_bytes());
        self.sequence_number = self.sequence_number.wrapping_add(1);
        packet.extend_from_slice(&frame.rtp_timestamp.to_be_bytes());
        packet.extend_from_slice(&self.sender_ssrc.to_be_bytes());
        packet.push(if frame.keyframe { 0xc0 } else { 0x40 });
        packet.push(frame.frame_id as u8);
        packet.extend_from_slice(&(packet_id as u16).to_be_bytes());
        packet.extend_from_slice(&((packet_count - 1) as u16).to_be_bytes());
        packet.push(frame.referenced_frame_id as u8);
        packet.extend_from_slice(payload);
        packet
    }

    fn send_sender_report(&mut self, rtp_timestamp: u32) -> Result<()> {
        let mut packet = Vec::with_capacity(28);
        packet.extend_from_slice(&[0x80, 200, 0, 6]);
        packet.extend_from_slice(&self.sender_ssrc.to_be_bytes());
        packet.extend_from_slice(&ntp_timestamp().to_be_bytes());
        packet.extend_from_slice(&rtp_timestamp.to_be_bytes());
        packet.extend_from_slice(&self.packet_count.to_be_bytes());
        packet.extend_from_slice(&self.octet_count.to_be_bytes());
        self.socket
            .send(&packet)
            .context("could not send Cast RTCP sender report")?;
        self.last_sender_report = Some(Instant::now());
        log::trace!(
            "sent RTCP sender report: rtp_timestamp={rtp_timestamp}, packets={}, octets={}",
            self.packet_count,
            self.octet_count
        );
        Ok(())
    }

    fn handle_rtcp(&mut self, packet: &[u8]) -> Result<()> {
        let mut offset = 0;
        while packet.len().saturating_sub(offset) >= 4 {
            let first = packet[offset];
            if first >> 6 != 2 {
                bail!("received malformed RTCP version");
            }
            let packet_type = packet[offset + 1];
            let payload_words = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]);
            let packet_size = 4 + usize::from(payload_words) * 4;
            if packet_size > packet.len() - offset {
                bail!("received truncated RTCP packet");
            }
            let part = &packet[offset..offset + packet_size];
            match (packet_type, first & 0x1f) {
                (206, 15) => self.handle_cast_feedback(part)?,
                (206, 1) if self.payload_type == RTP_H264_PAYLOAD_TYPE => {
                    log::debug!("receiver requested an H.264 recovery keyframe (PLI)");
                }
                (201, _) => log::trace!("received RTCP receiver report ({} bytes)", part.len()),
                (207, _) => log::trace!("received RTCP extended report ({} bytes)", part.len()),
                _ => log::trace!(
                    "received RTCP type {packet_type}, subtype {} ({} bytes)",
                    first & 0x1f,
                    part.len()
                ),
            }
            offset += packet_size;
        }
        Ok(())
    }

    fn handle_cast_feedback(&mut self, packet: &[u8]) -> Result<()> {
        if packet.len() < 20 {
            bail!("received truncated Cast RTCP feedback");
        }
        let receiver_ssrc = read_u32(packet, 4);
        let sender_ssrc = read_u32(packet, 8);
        if receiver_ssrc != self.receiver_ssrc || sender_ssrc != self.sender_ssrc {
            log::trace!("ignoring RTCP feedback for SSRCs {receiver_ssrc}/{sender_ssrc}");
            return Ok(());
        }
        if &packet[12..16] != b"CAST" {
            bail!("received RTCP feedback without CAST identifier");
        }

        let latest_frame = self.next_frame_id.wrapping_sub(1);
        let checkpoint = expand_frame_id_at_or_before(packet[16], latest_frame);
        let loss_count = usize::from(packet[17]);
        let playout_delay = u16::from_be_bytes([packet[18], packet[19]]);
        let required = 20 + loss_count * 4;
        if packet.len() < required {
            bail!("received truncated Cast RTCP loss fields");
        }
        self.stats
            .receiver_playout_delay_ms
            .store(u64::from(playout_delay), Ordering::Relaxed);
        let in_flight_frames_before_ack = self.history.len() as u64;
        let in_flight_bytes_before_ack = self
            .history
            .iter()
            .map(|frame| frame.encrypted_data.len() as u64)
            .sum::<u64>();
        let mut acknowledged_bytes = 0_u64;
        let mut checkpoint_pipeline_micros = None;
        // Before frame zero is complete, receivers use FrameId's all-ones
        // null sentinel as the checkpoint. It is not an acknowledgement.
        if checkpoint != u32::MAX {
            acknowledged_bytes = self
                .history
                .iter()
                .filter(|frame| frame.frame_id <= checkpoint)
                .map(|frame| frame.encrypted_data.len() as u64)
                .sum();
            if let Some(frame) = self
                .history
                .iter()
                .find(|frame| frame.frame_id == checkpoint)
            {
                let transport_micros =
                    u64::try_from(frame.sent_at.elapsed().as_micros()).unwrap_or(u64::MAX);
                let pipeline_micros =
                    u64::try_from(frame.pipeline_started_at.elapsed().as_micros())
                        .unwrap_or(u64::MAX);
                checkpoint_pipeline_micros = Some(pipeline_micros);
                self.stats
                    .checkpoint_samples
                    .fetch_add(1, Ordering::Relaxed);
                self.stats
                    .checkpoint_latency_micros
                    .fetch_add(transport_micros, Ordering::Relaxed);
                self.stats
                    .max_checkpoint_latency_micros
                    .fetch_max(transport_micros, Ordering::Relaxed);
                self.stats
                    .latency_samples
                    .lock()
                    .map_err(|_| anyhow!("latency sample lock was poisoned"))?
                    .push(LatencySample {
                        pipeline_micros,
                        transport_micros,
                        capture_age_micros: frame.capture_age_micros,
                        queue_wait_micros: frame.queue_wait_micros,
                        encode_micros: frame.encode_micros,
                        prepare_micros: frame.prepare_micros,
                        sender_lock_wait_micros: frame.sender_lock_wait_micros,
                        send_micros: frame.send_micros,
                        in_flight_frames: frame.in_flight_frames,
                        in_flight_bytes: frame.in_flight_bytes,
                        keyframe: frame.keyframe,
                        encoded_bytes: frame.encrypted_data.len() as u64,
                        synthetic_phase: frame.synthetic_phase,
                    });
            }
            self.history.retain(|frame| frame.frame_id > checkpoint);
            self.update_in_flight_stats();
        }

        let mut nacks = Vec::new();
        for index in 0..loss_count {
            let base = 20 + index * 4;
            let frame_id = expand_frame_id_after(packet[base], checkpoint);
            let packet_id = u16::from_be_bytes([packet[base + 1], packet[base + 2]]);
            nacks.push((frame_id, packet_id));
            if packet_id != u16::MAX {
                let mut bits = packet[base + 3];
                let mut additional = packet_id;
                while bits != 0 {
                    additional = additional.wrapping_add(1);
                    if bits & 1 != 0 {
                        nacks.push((frame_id, additional));
                    }
                    bits >>= 1;
                }
            }
        }
        self.stats.feedback_packets.fetch_add(1, Ordering::Relaxed);
        self.stats
            .nacks
            .fetch_add(nacks.len() as u64, Ordering::Relaxed);
        self.record_rate_control_window(
            acknowledged_bytes,
            checkpoint_pipeline_micros,
            nacks.len() as u64,
            in_flight_frames_before_ack,
            in_flight_bytes_before_ack,
        );
        log::trace!(
            "Cast RTCP feedback: checkpoint={checkpoint}, playout_delay={playout_delay}ms, nacks={}",
            nacks.len()
        );

        for (frame_id, packet_id) in nacks {
            let Some(frame) = self
                .history
                .iter()
                .find(|frame| frame.frame_id == frame_id)
                .cloned()
            else {
                log::debug!("cannot retransmit expired Cast frame {frame_id}");
                continue;
            };
            self.send_stored_frame(&frame, true, Some(packet_id))?;
        }
        Ok(())
    }

    fn record_rate_control_window(
        &mut self,
        acknowledged_bytes: u64,
        checkpoint_pipeline_micros: Option<u64>,
        nacks: u64,
        in_flight_frames: u64,
        in_flight_bytes: u64,
    ) {
        self.rate_window_acked_bytes = self
            .rate_window_acked_bytes
            .saturating_add(acknowledged_bytes);
        self.rate_window_nacks = self.rate_window_nacks.saturating_add(nacks);
        self.rate_window_max_latency_micros = self
            .rate_window_max_latency_micros
            .max(checkpoint_pipeline_micros.unwrap_or(0));
        self.rate_window_max_in_flight_frames =
            self.rate_window_max_in_flight_frames.max(in_flight_frames);
        self.rate_window_max_in_flight_bytes =
            self.rate_window_max_in_flight_bytes.max(in_flight_bytes);

        let elapsed = self.rate_window_started.elapsed();
        if elapsed < RATE_CONTROL_INTERVAL {
            return;
        }
        let acknowledged_bps = if elapsed.is_zero() {
            0
        } else {
            (u128::from(self.rate_window_acked_bytes) * 8 * 1_000_000_000 / elapsed.as_nanos())
                .try_into()
                .unwrap_or(u64::MAX)
        };
        let latency_limit = self
            .target_delay
            .saturating_mul(2)
            .max(Duration::from_millis(150));
        let current_bitrate = self.rate_control.target_bitrate();
        let byte_backlog_limit = current_bitrate
            .saturating_div(8)
            .saturating_mul(250)
            .saturating_div(1_000);
        let frame_backlog_limit = u64::from((self.fps / 4).max(3));
        let (congested, target_is_binding) = rate_window_is_congested(
            current_bitrate,
            acknowledged_bps,
            self.rate_window_nacks,
            self.rate_window_max_latency_micros,
            latency_limit,
            self.rate_window_max_in_flight_frames,
            frame_backlog_limit,
            self.rate_window_max_in_flight_bytes,
            byte_backlog_limit,
        );

        self.rate_control.report_window(
            self.rate_control_target,
            RateWindowHealth {
                congested,
                acknowledged_bps,
            },
        );
        log::trace!(
            "rate-control window: acked={:.2} Mbit/s, latency_max={:.1} ms, in_flight_max={} frames/{:.1} KiB, nacks={}, target_binding={target_is_binding}, congested={congested}, target={:.2} Mbit/s",
            acknowledged_bps as f64 / 1_000_000.0,
            micros_to_millis(self.rate_window_max_latency_micros),
            self.rate_window_max_in_flight_frames,
            self.rate_window_max_in_flight_bytes as f64 / 1024.0,
            self.rate_window_nacks,
            self.rate_control.target_bitrate() as f64 / 1_000_000.0,
        );
        self.rate_window_started = Instant::now();
        self.rate_window_acked_bytes = 0;
        self.rate_window_nacks = 0;
        self.rate_window_max_latency_micros = 0;
        self.rate_window_max_in_flight_frames = 0;
        self.rate_window_max_in_flight_bytes = 0;
    }
}

struct FeedbackThread {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FeedbackThread {
    fn start(
        ordinal: usize,
        host: IpAddr,
        socket: UdpSocket,
        senders: Vec<Arc<Mutex<CastRtpSender>>>,
        stats: Arc<MirrorStats>,
        failure: Arc<Mutex<Option<String>>>,
    ) -> Result<Self> {
        socket.set_read_timeout(Some(Duration::from_millis(200)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name(format!("cast-mirror-rtcp-{ordinal}"))
            .spawn(move || {
                let mut buffer = [0_u8; 2048];
                while !thread_stop.load(Ordering::SeqCst) {
                    match socket.recv(&mut buffer) {
                        Ok(size) => {
                            for sender in &senders {
                                if let Err(error) = sender
                                    .lock()
                                    .map_err(|_| anyhow!("Cast RTP sender lock was poisoned"))
                                    .and_then(|mut sender| sender.handle_rtcp(&buffer[..size]))
                                {
                                    store_source_failure(
                                        &failure,
                                        format!("Cast RTCP feedback from {host} failed: {error:#}"),
                                    );
                                    return;
                                }
                            }
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                ErrorKind::WouldBlock | ErrorKind::TimedOut
                            ) => {}
                        Err(error) => {
                            store_source_failure(
                                &failure,
                                format!("Cast RTCP feedback socket for {host} failed: {error}"),
                            );
                            break;
                        }
                    }
                }
                log::debug!(
                    "Cast RTCP feedback loop stopped after {} feedback packets and {} NACKs",
                    stats.feedback_packets.load(Ordering::Relaxed),
                    stats.nacks.load(Ordering::Relaxed)
                );
            })
            .context("could not start Cast RTCP feedback thread")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for FeedbackThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

enum RunningInput {
    Screen(SCStream, EncoderWorker, Option<AudioWorker>),
    Synthetic(SyntheticFrameSource, EncoderWorker),
}

impl RunningInput {
    fn stop_and_release(mut self) -> Result<()> {
        match &mut self {
            Self::Screen(stream, encoder, audio) => {
                let capture_result = stream
                    .stop_capture()
                    .context("could not stop screen capture");
                let encoder_result = encoder.stop();
                let audio_result = audio.take().map(AudioWorker::stop).transpose();
                capture_result?;
                encoder_result?;
                audio_result.map(|_| ())
            }
            Self::Synthetic(source, encoder) => {
                let source_result = source.stop();
                let encoder_result = encoder.stop();
                source_result?;
                encoder_result
            }
        }
    }
}

struct PendingFrame {
    surface: IOSurface,
    presentation_time: CMTime,
    pipeline_started_at: Instant,
    capture_age_micros: Option<u64>,
    synthetic_phase: Option<SyntheticPhase>,
    synthetic_recycle: Option<(mpsc::Sender<usize>, usize)>,
}

impl Drop for PendingFrame {
    fn drop(&mut self) {
        if let Some((sender, index)) = self.synthetic_recycle.take() {
            let _ = sender.send(index);
        }
    }
}

struct FrameQueueState {
    pending: Option<PendingFrame>,
    stopping: bool,
}

struct FrameQueue {
    state: Mutex<FrameQueueState>,
    available: Condvar,
    stats: Arc<MirrorStats>,
}

#[derive(Clone)]
struct FrameSubmitter {
    queue: Arc<FrameQueue>,
}

impl FrameSubmitter {
    fn submit(&self, frame: PendingFrame) -> Result<()> {
        let mut state = self
            .queue
            .state
            .lock()
            .map_err(|_| anyhow!("raw-frame queue lock was poisoned"))?;
        if state.stopping {
            bail!("raw-frame queue has stopped");
        }
        self.queue
            .stats
            .raw_frames_submitted
            .fetch_add(1, Ordering::Relaxed);
        if state.pending.replace(frame).is_some() {
            self.queue
                .stats
                .raw_frames_replaced
                .fetch_add(1, Ordering::Relaxed);
        }
        self.queue.available.notify_one();
        Ok(())
    }
}

struct EncoderWorker {
    queue: Arc<FrameQueue>,
    thread: Option<JoinHandle<()>>,
}

impl EncoderWorker {
    fn start(
        mut pipeline: MirrorPipeline,
        max_frame_age: Option<Duration>,
        failure: Arc<Mutex<Option<String>>>,
        stats: Arc<MirrorStats>,
    ) -> Result<(FrameSubmitter, Self)> {
        let queue = Arc::new(FrameQueue {
            state: Mutex::new(FrameQueueState {
                pending: None,
                stopping: false,
            }),
            available: Condvar::new(),
            stats: Arc::clone(&stats),
        });
        let thread_queue = Arc::clone(&queue);
        let thread = thread::Builder::new()
            .name("cast-mirror-encoder".into())
            .spawn(move || {
                loop {
                    let frame = {
                        let mut state = match thread_queue.state.lock() {
                            Ok(state) => state,
                            Err(_) => {
                                store_source_failure(
                                    &failure,
                                    "raw-frame queue lock was poisoned".to_owned(),
                                );
                                break;
                            }
                        };
                        while state.pending.is_none() && !state.stopping {
                            state = match thread_queue.available.wait(state) {
                                Ok(state) => state,
                                Err(_) => {
                                    store_source_failure(
                                        &failure,
                                        "raw-frame queue wait was poisoned".to_owned(),
                                    );
                                    return;
                                }
                            };
                        }
                        if state.stopping {
                            break;
                        }
                        state.pending.take().expect("pending frame was checked")
                    };

                    let queue_wait = frame.pipeline_started_at.elapsed();
                    if pipeline.has_reference_frame()
                        && max_frame_age.is_some_and(|deadline| queue_wait > deadline)
                    {
                        stats.raw_frames_expired.fetch_add(1, Ordering::Relaxed);
                        log::trace!(
                            "dropped raw frame after {:.1} ms queue wait",
                            queue_wait.as_secs_f64() * 1_000.0
                        );
                        continue;
                    }
                    if let Err(error) = pipeline.encode(
                        &frame.surface,
                        frame.presentation_time,
                        frame.pipeline_started_at,
                        frame.capture_age_micros,
                        duration_micros(queue_wait),
                        frame.synthetic_phase,
                    ) {
                        store_source_failure(
                            &failure,
                            format!("mirroring encode worker failed: {error:#}"),
                        );
                        if let Ok(mut state) = thread_queue.state.lock() {
                            state.stopping = true;
                            state.pending.take();
                        }
                        thread_queue.available.notify_all();
                        break;
                    }
                }
            })
            .context("could not start mirroring encoder worker")?;
        Ok((
            FrameSubmitter {
                queue: Arc::clone(&queue),
            },
            Self {
                queue,
                thread: Some(thread),
            },
        ))
    }

    fn stop(&mut self) -> Result<()> {
        if let Ok(mut state) = self.queue.state.lock() {
            state.stopping = true;
            state.pending.take();
        }
        self.queue.available.notify_all();
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("mirroring encoder worker panicked"))?;
        }
        Ok(())
    }
}

impl Drop for EncoderWorker {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct SyntheticFrameSource {
    stop_signal: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SyntheticFrameSource {
    fn start(
        submitter: FrameSubmitter,
        width: u32,
        height: u32,
        fps: u32,
        failure: Arc<Mutex<Option<String>>>,
        stats: Arc<MirrorStats>,
    ) -> Result<Self> {
        let mut generators = (0..SYNTHETIC_SURFACE_POOL_SIZE)
            .map(|_| SyntheticFrameGenerator::new(width, height, fps))
            .collect::<Result<Vec<_>>>()?;
        let stop_signal = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop_signal);
        let thread = thread::Builder::new()
            .name("cast-synthetic-frames".into())
            .spawn(move || {
                let (recycle_sender, recycle_receiver) = mpsc::channel();
                let started = Instant::now();
                let mut free_generators = (0..generators.len()).collect::<Vec<_>>();
                let mut frame_index = 0_u64;
                let mut last_phase = None;
                while !thread_stop.load(Ordering::SeqCst) {
                    let scheduled_at = started + frame_offset(frame_index, fps);
                    if let Some(wait) = scheduled_at.checked_duration_since(Instant::now()) {
                        thread::sleep(wait);
                    }
                    if thread_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    free_generators.extend(recycle_receiver.try_iter());
                    let Some(generator_index) = free_generators.pop() else {
                        stats
                            .synthetic_skipped_frames
                            .fetch_add(1, Ordering::Relaxed);
                        frame_index = frame_index.saturating_add(1);
                        continue;
                    };

                    let render_started = Instant::now();
                    let generator = &mut generators[generator_index];
                    let phase = match generator.render(frame_index) {
                        Ok(phase) => phase,
                        Err(error) => {
                            store_source_failure(
                                &failure,
                                format!("synthetic frame generation failed: {error:#}"),
                            );
                            break;
                        }
                    };
                    let render_micros = elapsed_micros(render_started);
                    let render_recorded = stats
                        .synthetic_render_micros
                        .lock()
                        .map(|mut samples| samples.push(render_micros));
                    if render_recorded.is_err() {
                        store_source_failure(
                            &failure,
                            "synthetic render timing lock was poisoned".to_owned(),
                        );
                        break;
                    }
                    if last_phase != Some(phase) {
                        log::debug!(
                            "synthetic workload entered {} phase at frame {frame_index}",
                            phase.label()
                        );
                        last_phase = Some(phase);
                    }

                    let presentation_value = match i64::try_from(frame_index) {
                        Ok(value) => value,
                        Err(_) => {
                            store_source_failure(
                                &failure,
                                "synthetic frame timestamp exceeded i64".to_owned(),
                            );
                            break;
                        }
                    };
                    let frame = PendingFrame {
                        surface: generator.surface().clone(),
                        presentation_time: CMTime::new(presentation_value, fps as i32),
                        pipeline_started_at: Instant::now(),
                        capture_age_micros: None,
                        synthetic_phase: Some(phase),
                        synthetic_recycle: Some((recycle_sender.clone(), generator_index)),
                    };
                    if let Err(error) = submitter.submit(frame) {
                        store_source_failure(
                            &failure,
                            format!("synthetic frame queue failed: {error:#}"),
                        );
                        break;
                    }
                    stats
                        .synthetic_generated_frames
                        .fetch_add(1, Ordering::Relaxed);

                    let next_frame = frame_index.saturating_add(1);
                    let frame_due_now = frame_at_elapsed(started.elapsed(), fps);
                    if frame_due_now > next_frame {
                        stats
                            .synthetic_skipped_frames
                            .fetch_add(frame_due_now - next_frame, Ordering::Relaxed);
                        frame_index = frame_due_now;
                    } else {
                        frame_index = next_frame;
                    }
                }
                log::debug!(
                    "synthetic frame source stopped after {} generated frames and {} skipped schedule slots",
                    stats.synthetic_generated_frames.load(Ordering::Relaxed),
                    stats.synthetic_skipped_frames.load(Ordering::Relaxed)
                );
            })
            .context("could not start synthetic frame source")?;
        Ok(Self {
            stop_signal,
            thread: Some(thread),
        })
    }

    fn stop(&mut self) -> Result<()> {
        self.stop_signal.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("synthetic frame source panicked"))?;
        }
        Ok(())
    }
}

impl Drop for SyntheticFrameSource {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn frame_offset(frame_index: u64, fps: u32) -> Duration {
    let nanoseconds = u128::from(frame_index)
        .saturating_mul(1_000_000_000)
        .checked_div(u128::from(fps.max(1)))
        .unwrap_or(0);
    Duration::from_nanos(u64::try_from(nanoseconds).unwrap_or(u64::MAX))
}

fn frame_at_elapsed(elapsed: Duration, fps: u32) -> u64 {
    let frame = elapsed
        .as_nanos()
        .saturating_mul(u128::from(fps))
        .checked_div(1_000_000_000)
        .unwrap_or(0);
    u64::try_from(frame).unwrap_or(u64::MAX)
}

fn store_source_failure(failure: &Mutex<Option<String>>, message: String) {
    if let Ok(mut failure) = failure.lock()
        && failure.is_none()
    {
        *failure = Some(message);
    }
}

struct MirrorFrameHandler {
    submitter: FrameSubmitter,
    failure: Arc<Mutex<Option<String>>>,
    last_surface: Mutex<Option<IOSurface>>,
    repeated_samples: AtomicU64,
    skipped_samples: AtomicU64,
}

impl SCStreamOutputTrait for MirrorFrameHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, output_type: SCStreamOutputType) {
        if output_type != SCStreamOutputType::Screen {
            return;
        }

        let result = (|| -> Result<()> {
            let pipeline_started_at = Instant::now();
            let status = sample.frame_status();
            let presentation_time = sample.output_presentation_timestamp();
            let capture_age_micros = sample.display_time().and_then(capture_age_micros);
            let (fresh_surface, missing_reason) = match sample.image_buffer() {
                Some(pixel_buffer) => match pixel_buffer.io_surface() {
                    Some(surface) => (Some(surface), None),
                    None => (None, Some("pixel buffer is not IOSurface-backed")),
                },
                None if status.is_some_and(|status| !status.has_content()) => {
                    (None, Some("frame has no new screen content"))
                }
                None => (None, Some("sample has no pixel buffer")),
            };
            let surface = if let Some(surface) = fresh_surface {
                *self
                    .last_surface
                    .lock()
                    .map_err(|_| anyhow!("last screen surface lock was poisoned"))? =
                    Some(surface.clone());
                surface
            } else {
                let Some(surface) = self
                    .last_surface
                    .lock()
                    .map_err(|_| anyhow!("last screen surface lock was poisoned"))?
                    .clone()
                else {
                    self.record_skipped_sample(
                        status,
                        missing_reason.unwrap_or("no reusable screen surface is available"),
                    );
                    return Ok(());
                };
                self.record_repeated_sample(
                    status,
                    missing_reason.unwrap_or("frame reused the previous surface"),
                );
                surface
            };
            self.submitter.submit(PendingFrame {
                surface,
                presentation_time,
                pipeline_started_at,
                capture_age_micros,
                synthetic_phase: None,
                synthetic_recycle: None,
            })?;
            Ok(())
        })();

        if let Err(error) = result
            && let Ok(mut failure) = self.failure.lock()
            && failure.is_none()
        {
            *failure = Some(format!("{error:#}"));
        }
    }
}

impl MirrorFrameHandler {
    fn record_repeated_sample(
        &self,
        status: Option<screencapturekit::SCFrameStatus>,
        reason: &str,
    ) {
        let count = self.repeated_samples.fetch_add(1, Ordering::Relaxed) + 1;
        if count == 1 || count.is_multiple_of(120) {
            log::debug!(
                "encoded {count} idle mirroring samples by reusing the last frame (latest status={status:?}: {reason})"
            );
        }
    }

    fn record_skipped_sample(&self, status: Option<screencapturekit::SCFrameStatus>, reason: &str) {
        let count = self.skipped_samples.fetch_add(1, Ordering::Relaxed) + 1;
        if count == 1 || count.is_multiple_of(120) {
            log::debug!(
                "skipped {count} non-video mirroring samples (latest status={status:?}: {reason})"
            );
        }
    }
}

const SENDER_QUEUE_FRAMES: usize = 3;

#[derive(Clone)]
struct EncodedFrame {
    rtp_timestamp: u32,
    keyframe: bool,
    data: Arc<Vec<u8>>,
    timings: FrameTimings,
    synthetic_phase: Option<SyntheticPhase>,
}

#[derive(Clone)]
struct SenderSubmitter {
    host: IpAddr,
    sender: mpsc::SyncSender<EncodedFrame>,
}

impl SenderSubmitter {
    fn submit(&self, frame: EncodedFrame) -> Result<()> {
        match self.sender.try_send(frame) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => bail!(
                "Cast receiver {} fell more than {SENDER_QUEUE_FRAMES} encoded frames behind",
                self.host
            ),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                bail!("Cast sender worker for {} stopped unexpectedly", self.host)
            }
        }
    }
}

struct SenderWorker {
    thread: Option<JoinHandle<()>>,
}

impl SenderWorker {
    fn start(
        ordinal: usize,
        host: IpAddr,
        sender: Arc<Mutex<CastRtpSender>>,
        failure: Arc<Mutex<Option<String>>>,
    ) -> Result<(SenderSubmitter, Self)> {
        let (submitter, receiver) = mpsc::sync_channel::<EncodedFrame>(SENDER_QUEUE_FRAMES);
        let thread = thread::Builder::new()
            .name(format!("cast-mirror-send-{ordinal}"))
            .spawn(move || {
                while let Ok(frame) = receiver.recv() {
                    let lock_started = Instant::now();
                    let result = sender
                        .lock()
                        .map_err(|_| anyhow!("Cast RTP sender lock was poisoned"))
                        .and_then(|mut sender| {
                            let mut timings = frame.timings;
                            timings.sender_lock_wait_micros = elapsed_micros(lock_started);
                            sender.send_frame(
                                frame.rtp_timestamp,
                                frame.keyframe,
                                &frame.data,
                                timings,
                                frame.synthetic_phase,
                            )
                        });
                    if let Err(error) = result {
                        store_source_failure(
                            &failure,
                            format!("Cast sender for {host} failed: {error:#}"),
                        );
                        break;
                    }
                }
            })
            .with_context(|| format!("could not start Cast sender worker for {host}"))?;
        Ok((
            SenderSubmitter {
                host,
                sender: submitter,
            },
            Self {
                thread: Some(thread),
            },
        ))
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("Cast sender worker panicked"))?;
        }
        Ok(())
    }
}

impl Drop for SenderWorker {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct MirrorPipeline {
    encoder: CompressionSession,
    outputs: Vec<SenderSubmitter>,
    parameter_sets: Option<(Vec<u8>, Vec<u8>)>,
    frame_index: u64,
    clock: Arc<MediaClock>,
    last_timestamp: Option<u64>,
    fps: u32,
    rate_control: Arc<AdaptiveRateControl>,
    encoder_bitrate: i32,
    stats: Arc<MirrorStats>,
}

impl MirrorPipeline {
    fn has_reference_frame(&self) -> bool {
        self.parameter_sets.is_some()
    }

    fn encode(
        &mut self,
        surface: &IOSurface,
        presentation_time: CMTime,
        pipeline_started_at: Instant,
        capture_age_micros: Option<u64>,
        queue_wait_micros: u64,
        synthetic_phase: Option<SyntheticPhase>,
    ) -> Result<()> {
        self.apply_requested_bitrate();
        let timestamp = self.normalized_timestamp(presentation_time);
        let encoder_time = (
            i64::try_from(timestamp)
                .context("mirroring timestamp exceeded VideoToolbox's range")?,
            RTP_VIDEO_TIMEBASE as i32,
        );
        let encode_started = Instant::now();
        let encoded = self
            .encoder
            .encode(surface, encoder_time)
            .context("VideoToolbox could not encode a mirroring frame")?;
        let encode_micros = elapsed_micros(encode_started);
        if encoded.data.is_empty() {
            return Ok(());
        }

        let prepare_started = Instant::now();
        let keyframe = avcc_contains_nal_type(&encoded.data, 5)?;
        if keyframe {
            let sets = h264_parameter_sets(encoded.cm_sample_buffer_ptr().cast())?;
            log::trace!(
                "mirroring keyframe parameter sets: SPS={} bytes, PPS={} bytes",
                sets.0.len(),
                sets.1.len()
            );
            self.parameter_sets = Some(sets);
        }
        let Some((sps, pps)) = self.parameter_sets.as_ref() else {
            log::debug!("discarding H.264 mirroring frame before the first keyframe");
            return Ok(());
        };
        let annex_b = avcc_to_annex_b(&encoded.data, keyframe.then_some((sps, pps)))?;
        let prepare_micros = elapsed_micros(prepare_started);
        let frame = EncodedFrame {
            rtp_timestamp: timestamp as u32,
            keyframe,
            data: Arc::new(annex_b),
            timings: FrameTimings {
                pipeline_started_at,
                capture_age_micros,
                queue_wait_micros,
                encode_micros,
                prepare_micros,
                sender_lock_wait_micros: 0,
            },
            synthetic_phase,
        };
        for output in &self.outputs {
            output.submit(frame.clone())?;
        }
        self.frame_index += 1;
        if keyframe || self.frame_index.is_multiple_of(self.fps as u64) {
            log::debug!(
                "sent mirroring frame {}: {} Annex-B bytes, keyframe={keyframe}, rtp_timestamp={timestamp}",
                self.frame_index,
                frame.data.len()
            );
        }
        Ok(())
    }

    fn apply_requested_bitrate(&mut self) {
        let target = self.rate_control.target_bitrate().min(i32::MAX as u64) as i32;
        if target == self.encoder_bitrate {
            return;
        }
        if let Err(error) = set_encoder_i32(
            &self.encoder,
            unsafe { videotoolbox::ffi::kVTCompressionPropertyKey_AverageBitRate },
            target,
        ) {
            self.stats
                .adaptive_bitrate_apply_failures
                .fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "VideoToolbox rejected adaptive bitrate {:.2} Mbit/s: {error:#}",
                target as f64 / 1_000_000.0
            );
        }
        self.encoder_bitrate = target;
    }

    fn normalized_timestamp(&mut self, presentation_time: CMTime) -> u64 {
        let fallback_step = RTP_VIDEO_TIMEBASE / self.fps as u64;
        let candidate = self
            .clock
            .ticks(presentation_time, RTP_VIDEO_TIMEBASE)
            .unwrap_or_else(|| {
                self.last_timestamp
                    .map_or(0, |last| last.saturating_add(fallback_step))
            });
        let timestamp = match self.last_timestamp {
            Some(last) if candidate <= last => last.saturating_add(fallback_step),
            _ => candidate,
        };
        self.last_timestamp = Some(timestamp);
        timestamp
    }
}

fn encrypt_frame(data: &[u8], frame_id: u32, key: &[u8; 16], iv_mask: &[u8; 16]) -> Vec<u8> {
    let mut nonce = *iv_mask;
    for (target, byte) in nonce[8..12].iter_mut().zip(frame_id.to_be_bytes()) {
        *target ^= byte;
    }
    let mut output = data.to_vec();
    let mut cipher = Aes128Ctr::new(key.into(), (&nonce).into());
    cipher.apply_keystream(&mut output);
    output
}

fn avcc_to_annex_b(data: &[u8], parameter_sets: Option<(&Vec<u8>, &Vec<u8>)>) -> Result<Vec<u8>> {
    const START_CODE: [u8; 4] = [0, 0, 0, 1];
    let parameter_bytes =
        parameter_sets.map_or(0, |(sps, pps)| START_CODE.len() * 2 + sps.len() + pps.len());
    let mut output = Vec::with_capacity(data.len() + parameter_bytes);
    if let Some((sps, pps)) = parameter_sets {
        output.extend_from_slice(&START_CODE);
        output.extend_from_slice(sps);
        output.extend_from_slice(&START_CODE);
        output.extend_from_slice(pps);
    }

    let mut offset = 0;
    while offset < data.len() {
        if data.len() - offset < 4 {
            bail!("truncated AVCC NAL length");
        }
        let length = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        if length == 0 || length > data.len() - offset {
            bail!("invalid AVCC NAL length {length}");
        }
        output.extend_from_slice(&START_CODE);
        output.extend_from_slice(&data[offset..offset + length]);
        offset += length;
    }
    Ok(output)
}

fn avcc_contains_nal_type(data: &[u8], wanted: u8) -> Result<bool> {
    let mut offset = 0;
    while offset < data.len() {
        if data.len() - offset < 4 {
            bail!("truncated AVCC NAL length");
        }
        let length = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        if length == 0 || length > data.len() - offset {
            bail!("invalid AVCC NAL length {length}");
        }
        if data[offset] & 0x1f == wanted {
            return Ok(true);
        }
        offset += length;
    }
    Ok(false)
}

fn h264_parameter_sets(sample: *mut c_void) -> Result<(Vec<u8>, Vec<u8>)> {
    if sample.is_null() {
        bail!("encoded mirroring keyframe had no CoreMedia sample buffer");
    }
    let format = unsafe { CMSampleBufferGetFormatDescription(sample) };
    if format.is_null() {
        bail!("encoded mirroring keyframe had no H.264 format description");
    }

    let read = |index| -> Result<Vec<u8>> {
        let mut pointer = std::ptr::null();
        let mut size = 0_usize;
        let mut count = 0_usize;
        let mut header_length = 0_i32;
        let status = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format,
                index,
                &mut pointer,
                &mut size,
                &mut count,
                &mut header_length,
            )
        };
        if status != 0 || pointer.is_null() || size == 0 {
            bail!("CoreMedia could not read H.264 parameter set {index} (status {status})");
        }
        Ok(unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec())
    };
    Ok((read(0)?, read(1)?))
}

fn local_ip_for(host: IpAddr, port: u16) -> Result<IpAddr> {
    let bind = if host.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind).context("could not create route probe socket")?;
    socket
        .connect(SocketAddr::new(host, port))
        .with_context(|| format!("could not find a network route to {host}"))?;
    Ok(socket.local_addr()?.ip())
}

fn take_failure(failure: &Mutex<Option<String>>) -> Result<Option<String>> {
    Ok(failure
        .lock()
        .map_err(|_| anyhow!("failure state lock was poisoned"))?
        .take())
}

fn ntp_timestamp() -> u64 {
    const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = elapsed.as_secs().wrapping_add(NTP_UNIX_OFFSET) as u32;
    let fraction = ((u64::from(elapsed.subsec_nanos()) << 32) / 1_000_000_000) as u32;
    (u64::from(seconds) << 32) | u64::from(fraction)
}

fn expand_frame_id_at_or_before(truncated: u8, maximum: u32) -> u32 {
    let mut candidate = (maximum & !0xff) | u32::from(truncated);
    if candidate > maximum {
        candidate = candidate.wrapping_sub(256);
    }
    candidate
}

fn expand_frame_id_after(truncated: u8, checkpoint: u32) -> u32 {
    let mut candidate = (checkpoint & !0xff) | u32::from(truncated);
    if candidate <= checkpoint {
        candidate = candidate.wrapping_add(256);
    }
    candidate
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

const fn even(value: u32) -> u32 {
    value - value % 2
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

fn capture_age_micros(display_time: u64) -> Option<u64> {
    let now = unsafe { mach_absolute_time() };
    let elapsed_ticks = now.checked_sub(display_time)?;
    static TIMEBASE: OnceLock<Option<MachTimebaseInfo>> = OnceLock::new();
    let timebase = TIMEBASE
        .get_or_init(|| {
            let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
            (unsafe { mach_timebase_info(&mut info) } == 0 && info.denom != 0).then_some(info)
        })
        .as_ref()?;
    let nanoseconds = u128::from(elapsed_ticks).saturating_mul(u128::from(timebase.numer))
        / u128::from(timebase.denom);
    u64::try_from(nanoseconds / 1_000).ok()
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

#[cfg_attr(target_os = "macos", link(name = "CoreMedia", kind = "framework"))]
unsafe extern "C" {
    fn CMSampleBufferGetFormatDescription(sample_buffer: *mut c_void) -> *mut c_void;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        video_description: *mut c_void,
        parameter_set_index: usize,
        parameter_set_pointer: *mut *const u8,
        parameter_set_size: *mut usize,
        parameter_set_count: *mut usize,
        nal_unit_header_length: *mut i32,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::{
        AdaptiveRateControl, CastRtpSender, CastStreamingControlState, CastStreamingOfferMessage,
        EncodedFrame, FrameTimings, H264Level, LatencyDistribution, LatencyRecommendations,
        LiveLatencyGraph, MirrorStats, NegotiatedTarget, NegotiatedTransport, OfferMaterial,
        ProfileRunResult, RTP_H264_PAYLOAD_TYPE, RateWindowHealth, RawFrameDeadline,
        SENDER_QUEUE_FRAMES, SenderSubmitter, StoredFrame, StreamMaterial, TuningConfig,
        aggregate_profile_results, answer_display_frame_rate, auto_tune_score, avcc_to_annex_b,
        combined_tuning_config, encrypt_frame, expand_frame_id_after, expand_frame_id_at_or_before,
        frame_pacing, host_command_arguments, negotiated_group_frame_rate,
        rate_window_is_congested, recommend_latency, round_target_delay, select_tuning_winner,
        source_command_argument, validate_cast_hosts,
    };
    use crate::audio;
    use serde_json::json;
    use std::{
        net::{IpAddr, UdpSocket},
        sync::{Arc, mpsc},
        time::{Duration, Instant},
    };

    #[test]
    fn converts_avcc_to_annex_b_and_prepends_parameter_sets() {
        let avcc = [0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 1, 0x41];
        let sps = vec![0x67, 0x42];
        let pps = vec![0x68, 0xce];
        assert_eq!(
            avcc_to_annex_b(&avcc, Some((&sps, &pps))).unwrap(),
            [
                0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0xaa, 0, 0, 0, 1,
                0x41,
            ]
        );
    }

    #[test]
    fn profile_recommendation_preserves_the_capture_source() {
        assert_eq!(source_command_argument(true, Some(42)), " --extend");
        assert_eq!(source_command_argument(false, Some(42)), " --display 42");
        assert_eq!(source_command_argument(false, None), "");
        assert_eq!(
            host_command_arguments(&["192.0.2.1".parse().unwrap(), "192.0.2.2".parse().unwrap(),]),
            " --host 192.0.2.1 --host 192.0.2.2"
        );
    }

    #[test]
    fn cast_frame_encryption_round_trips() {
        let key = [0x11; 16];
        let mask = [0x22; 16];
        let plaintext = b"a Cast Streaming H.264 frame";
        let encrypted = encrypt_frame(plaintext, 513, &key, &mask);
        assert_ne!(encrypted, plaintext);
        assert_eq!(encrypt_frame(&encrypted, 513, &key, &mask), plaintext);
        assert_ne!(
            encrypted,
            encrypt_frame(plaintext, 514, &key, &mask),
            "the frame ID must alter the AES-CTR nonce"
        );
    }

    #[test]
    fn expands_truncated_cast_frame_ids_across_wraparound() {
        assert_eq!(expand_frame_id_at_or_before(4, 260), 260);
        assert_eq!(expand_frame_id_at_or_before(250, 260), 250);
        assert_eq!(expand_frame_id_after(5, 260), 261);
        assert_eq!(expand_frame_id_after(250, 260), 506);
    }

    #[test]
    fn computes_nearest_rank_latency_percentiles() {
        let distribution =
            LatencyDistribution::from_values((1_u64..=100).map(|value| value * 1_000).collect())
                .unwrap();
        assert_eq!(distribution.count, 100);
        assert_eq!(distribution.average_micros, 50_500);
        assert_eq!(distribution.p50_micros, 50_000);
        assert_eq!(distribution.p95_micros, 95_000);
        assert_eq!(distribution.p99_micros, 99_000);
        assert_eq!(distribution.max_micros, 100_000);
    }

    #[test]
    fn recommends_latency_with_frame_and_loss_headroom() {
        let distribution = LatencyDistribution {
            count: 1_000,
            average_micros: 30_000,
            p50_micros: 20_000,
            p95_micros: 70_000,
            p99_micros: 110_000,
            max_micros: 150_000,
        };
        assert_eq!(
            recommend_latency(distribution, 30, 6, 4_500),
            LatencyRecommendations {
                aggressive_ms: 100,
                balanced_ms: 200,
                resilient_ms: 250,
            }
        );
        assert_eq!(round_target_delay(151), 200);
        assert_eq!(round_target_delay(5_001), 5_000);
    }

    #[test]
    fn auto_tune_score_penalizes_tail_loss_drops_and_frame_rate_shortfall() {
        let distribution = LatencyDistribution {
            count: 100,
            average_micros: 80_000,
            p50_micros: 70_000,
            p95_micros: 100_000,
            p99_micros: 140_000,
            max_micros: 180_000,
        };
        assert_eq!(auto_tune_score(distribution, 2.0, 3.0, 4.0), 144.0);
    }

    #[test]
    fn combines_only_independent_improvements_outside_the_noise_margin() {
        assert_eq!(
            combined_tuning_config(100.0, 90.0, 96.0, 92.0, 90.0),
            TuningConfig {
                adaptive_bitrate: false,
                prioritize_encoding_speed: true,
                raw_frame_deadline: RawFrameDeadline::Disabled,
            }
        );
        assert_eq!(
            combined_tuning_config(100.0, 95.0, 95.0, 95.0, 110.0),
            TuningConfig::BASELINE,
            "an exact five-millisecond difference remains inside the noise margin"
        );
    }

    #[test]
    fn winner_prefers_defaults_when_scores_are_within_noise_margin() {
        let fixed = TuningConfig {
            adaptive_bitrate: false,
            ..TuningConfig::BASELINE
        };
        assert_eq!(
            select_tuning_winner(&[(TuningConfig::BASELINE, 103.0), (fixed, 100.0)]),
            0
        );
        assert_eq!(
            select_tuning_winner(&[(TuningConfig::BASELINE, 106.0), (fixed, 100.0)]),
            1
        );
    }

    #[test]
    fn tuning_arguments_reproduce_the_profiled_controls() {
        let config = TuningConfig {
            adaptive_bitrate: false,
            prioritize_encoding_speed: false,
            raw_frame_deadline: RawFrameDeadline::OneFrame,
        };
        assert_eq!(
            config.command_arguments(30),
            " --max-frame-age-ms 34 --fixed-bitrate --quality-priority"
        );
    }

    #[test]
    fn renders_a_fixed_width_latency_sparkline() {
        let mut graph = LiveLatencyGraph::new(Duration::from_secs(60), false, 30);
        graph.history.extend([Some(10), Some(25), None, Some(50)]);
        let (sparkline, scale) = graph.sparkline();
        assert_eq!(sparkline.chars().count(), 44);
        assert_eq!(scale, 50);
        assert!(sparkline.ends_with("▃▅·█"));
    }

    #[test]
    fn selects_a_valid_h264_level_for_720p60() {
        assert_eq!(
            H264Level::for_stream(1280, 720, 30, 6_000_000)
                .unwrap()
                .name,
            "3.1"
        );
        assert_eq!(
            H264Level::for_stream(1280, 720, 60, 6_000_000)
                .unwrap()
                .name,
            "3.2"
        );
        assert_eq!(
            H264Level::for_stream(1920, 1080, 30, 6_000_000)
                .unwrap()
                .name,
            "4.0"
        );
    }

    #[test]
    fn reads_the_receiver_selected_frame_rate() {
        assert_eq!(
            answer_display_frame_rate(&json!({
                "dimensions": { "frameRate": "30" }
            })),
            Some(30)
        );
        assert_eq!(
            answer_display_frame_rate(&json!({
                "dimensions": { "frameRate": "60/1" }
            })),
            Some(60)
        );
    }

    #[test]
    fn audio_offer_keeps_audio_at_index_zero_and_video_at_one() {
        let material = OfferMaterial {
            sequence_number: 1,
            sender_ssrc: 50_001,
            aes_key: [1; 16],
            aes_iv_mask: [2; 16],
            audio: Some(StreamMaterial {
                sender_ssrc: 100_001,
                aes_key: [3; 16],
                aes_iv_mask: [4; 16],
            }),
        };
        let offer = CastStreamingOfferMessage::new(
            &material,
            1280,
            720,
            30,
            6_000_000,
            Duration::from_millis(200),
            H264Level::for_stream(1280, 720, 30, 6_000_000).unwrap(),
        );
        let value = serde_json::to_value(offer).unwrap();
        let streams = value["offer"]["supportedStreams"].as_array().unwrap();
        assert_eq!(streams[0]["index"], 0);
        assert_eq!(streams[0]["type"], "audio_source");
        assert_eq!(streams[0]["codecName"], "aac");
        assert_eq!(streams[0]["codecParameter"], "mp4a.40.2");
        assert_eq!(streams[0]["bitRate"], audio::BITRATE);
        assert_eq!(streams[0]["rtpPayloadType"], audio::RTP_PAYLOAD_TYPE);
        assert_eq!(streams[1]["index"], 1);
        assert_eq!(streams[1]["type"], "video_source");
        assert_eq!(streams[1]["rtpPayloadType"], RTP_H264_PAYLOAD_TYPE);
    }

    #[test]
    fn bounds_frame_aware_packet_pacing() {
        assert!(frame_pacing(16, false, 30).is_none());
        let (_, delta_interval) = frame_pacing(17, false, 30).unwrap();
        assert!(delta_interval <= Duration::from_millis(5));
        assert!(frame_pacing(24, true, 30).is_none());
        let (_, keyframe_interval) = frame_pacing(49, true, 30).unwrap();
        assert!(keyframe_interval <= Duration::from_millis(3));
    }

    #[test]
    fn adaptive_rate_control_decreases_fast_and_increases_slowly() {
        let stats = Arc::new(MirrorStats::default());
        let rate = AdaptiveRateControl::new(6_000_000, true, Arc::clone(&stats));
        rate.decrease(3_000_000);
        assert_eq!(rate.target_bitrate(), 4_800_000);
        rate.increase();
        assert_eq!(rate.target_bitrate(), 5_040_000);
    }

    #[test]
    fn grouped_rate_control_uses_the_worst_receiver() {
        let stats = Arc::new(MirrorStats::default());
        let rate = AdaptiveRateControl::new_group(6_000_000, true, stats, 2);
        let healthy = RateWindowHealth {
            congested: false,
            acknowledged_bps: 6_000_000,
        };
        let congested = RateWindowHealth {
            congested: true,
            acknowledged_bps: 3_000_000,
        };

        rate.report_window(0, healthy);
        assert_eq!(rate.target_bitrate(), 6_000_000);
        rate.report_window(1, congested);
        assert_eq!(rate.target_bitrate(), 4_800_000);

        for _ in 0..3 {
            rate.report_window(0, healthy);
            rate.report_window(1, healthy);
        }
        assert_eq!(rate.target_bitrate(), 5_040_000);
    }

    #[test]
    fn sender_fanout_fails_when_a_receiver_stalls_or_disconnects() {
        let frame = EncodedFrame {
            rtp_timestamp: 1,
            keyframe: false,
            data: Arc::new(vec![1, 2, 3]),
            timings: FrameTimings {
                pipeline_started_at: Instant::now(),
                capture_age_micros: None,
                queue_wait_micros: 0,
                encode_micros: 0,
                prepare_micros: 0,
                sender_lock_wait_micros: 0,
            },
            synthetic_phase: None,
        };
        let host: IpAddr = "192.0.2.1".parse().unwrap();
        let (sender, receiver) = mpsc::sync_channel(SENDER_QUEUE_FRAMES);
        let output = SenderSubmitter { host, sender };
        for _ in 0..SENDER_QUEUE_FRAMES {
            output.submit(frame.clone()).unwrap();
        }
        let error = output.submit(frame.clone()).unwrap_err().to_string();
        assert!(error.contains("fell more than"));
        drop(receiver);

        let (sender, receiver) = mpsc::sync_channel(SENDER_QUEUE_FRAMES);
        drop(receiver);
        let output = SenderSubmitter { host, sender };
        let error = output.submit(frame).unwrap_err().to_string();
        assert!(error.contains("stopped unexpectedly"));
    }

    #[test]
    fn shared_capture_uses_the_slowest_negotiated_frame_rate() {
        let target = |host: &str, receiver_frame_rate| NegotiatedTarget {
            host: host.parse().unwrap(),
            port: 8009,
            transport: NegotiatedTransport {
                udp_port: 50_000,
                session_id: String::new(),
                sender_ssrc: 1,
                receiver_ssrc: 2,
                aes_key: [0; 16],
                aes_iv_mask: [0; 16],
                audio: None,
                receiver_frame_rate,
                control: Arc::new(CastStreamingControlState::default()),
            },
            stopped: true,
        };
        let targets = [
            target("192.0.2.1", Some(60)),
            target("192.0.2.2", Some(30)),
            target("192.0.2.3", None),
        ];
        assert_eq!(negotiated_group_frame_rate(&targets, 60), 30);
    }

    #[test]
    fn profile_group_aggregation_keeps_worst_receiver_metrics() {
        let profile = |p95_micros: u64,
                       p99_micros: u64,
                       balanced_ms: u64,
                       retransmission_percent: f64,
                       measured_fps: f64,
                       effective_fps: u32,
                       score_ms: f64| ProfileRunResult {
            sampled_for: Duration::from_secs(10),
            pipeline: LatencyDistribution {
                count: 100,
                average_micros: p95_micros / 2,
                p50_micros: p95_micros / 3,
                p95_micros,
                p99_micros,
                max_micros: p99_micros + 10_000,
            },
            recommendations: LatencyRecommendations {
                aggressive_ms: balanced_ms.saturating_sub(50),
                balanced_ms,
                resilient_ms: balanced_ms + 50,
            },
            retransmission_percent,
            raw_drop_percent: retransmission_percent / 2.0,
            frame_rate_shortfall_percent: 30.0 - measured_fps,
            measured_fps,
            requested_fps: 60,
            effective_fps,
            final_target_bitrate: u64::from(effective_fps) * 100_000,
            score_ms,
        };
        let aggregate = aggregate_profile_results(&[
            profile(80_000, 100_000, 150, 0.1, 59.0, 60, 90.0),
            profile(120_000, 180_000, 250, 2.0, 28.0, 30, 160.0),
        ])
        .unwrap();
        assert_eq!(aggregate.pipeline.p95_micros, 120_000);
        assert_eq!(aggregate.pipeline.p99_micros, 180_000);
        assert_eq!(aggregate.recommendations.balanced_ms, 250);
        assert_eq!(aggregate.retransmission_percent, 2.0);
        assert_eq!(aggregate.measured_fps, 28.0);
        assert_eq!(aggregate.effective_fps, 30);
        assert_eq!(aggregate.final_target_bitrate, 3_000_000);
        assert_eq!(aggregate.score_ms, 160.0);
    }

    #[test]
    fn receiver_groups_reject_duplicate_hosts() {
        let host: IpAddr = "192.0.2.1".parse().unwrap();
        assert!(validate_cast_hosts(&[]).is_err());
        assert!(validate_cast_hosts(&[host, host]).is_err());
        assert!(validate_cast_hosts(&[host]).is_ok());
    }

    #[test]
    fn congestion_detection_does_not_lower_an_unused_bitrate_ceiling() {
        let (congested, binding) = rate_window_is_congested(
            6_000_000,
            900_000,
            20,
            180_000,
            Duration::from_millis(150),
            6,
            7,
            100 * 1024,
            180 * 1024,
        );
        assert!(!binding);
        assert!(!congested);

        let (congested, binding) = rate_window_is_congested(
            6_000_000,
            4_000_000,
            3,
            100_000,
            Duration::from_millis(150),
            3,
            7,
            80 * 1024,
            180 * 1024,
        );
        assert!(binding);
        assert!(congested);
    }

    #[test]
    fn packetizes_a_cast_h264_frame() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.connect(receiver.local_addr().unwrap()).unwrap();
        let stats = Arc::new(MirrorStats::default());
        let rate_control = Arc::new(AdaptiveRateControl::new(
            6_000_000,
            true,
            Arc::clone(&stats),
        ));
        let mut sender = CastRtpSender::new(
            socket,
            true,
            50_001,
            50_002,
            [1; 16],
            [2; 16],
            stats,
            rate_control,
            0,
            30,
            Duration::from_millis(100),
        )
        .unwrap();
        let frame = StoredFrame {
            frame_id: 0,
            referenced_frame_id: 0,
            rtp_timestamp: 90_000,
            keyframe: true,
            encrypted_data: vec![7; 2_000],
            pipeline_started_at: std::time::Instant::now(),
            sent_at: std::time::Instant::now(),
            capture_age_micros: None,
            queue_wait_micros: 0,
            encode_micros: 0,
            prepare_micros: 0,
            sender_lock_wait_micros: 0,
            send_micros: 0,
            in_flight_frames: 1,
            in_flight_bytes: 2_000,
            synthetic_phase: None,
        };
        let packet = sender.packetize(&frame, 0, 2, 1_449);
        assert_eq!(packet[0], 0x80);
        assert_eq!(packet[1], 101);
        assert_eq!(&packet[4..8], &90_000_u32.to_be_bytes());
        assert_eq!(&packet[8..12], &50_001_u32.to_be_bytes());
        assert_eq!(packet[12], 0xc0);
        assert_eq!(packet[13], 0);
        assert_eq!(&packet[14..16], &[0, 0]);
        assert_eq!(&packet[16..18], &[0, 1]);
        assert_eq!(packet[18], 0);
        assert_eq!(packet.len(), 19 + 1_449);
    }
}
