use std::{
    fs::File,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    cast::{
        BufferedMediaSession, MediaFailure, MediaFailureKind, MediaSessionEvent, PlaybackEnd,
        PlaybackState,
    },
    media::{self, CompatibilityMode, PreparationPlan},
    media_server::MediaFileServer,
    network::{local_ip_for, private_route},
    vod_hls::IncrementalHlsPreparation,
};

const PLAYBACK_START_TIMEOUT: Duration = Duration::from_secs(20);
const INCREMENTAL_PREPARATION_TIMEOUT: Duration = Duration::from_secs(120);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TranscodeDelivery {
    Complete,
    #[default]
    Incremental,
}

pub struct VideoOptions {
    pub cast_host: std::net::IpAddr,
    pub cast_port: u16,
    pub http_port: u16,
    pub file: PathBuf,
    pub start_at: f64,
    pub content_type: Option<String>,
    pub compatibility_mode: CompatibilityMode,
    pub transcode_delivery: TranscodeDelivery,
}

#[derive(Clone, Copy)]
struct IncrementalCastTarget {
    cast_host: std::net::IpAddr,
    cast_port: u16,
    http_port: u16,
    start_at: f64,
}

pub fn cast_video(options: VideoOptions) -> Result<()> {
    validate_start_at(options.start_at)?;
    let path = options
        .file
        .canonicalize()
        .with_context(|| format!("could not resolve local video {}", options.file.display()))?;
    let source_file = File::open(&path)
        .with_context(|| format!("could not open local video {}", path.display()))?;
    let metadata = source_file
        .metadata()
        .with_context(|| format!("could not inspect local video {}", path.display()))?;
    if !metadata.is_file() {
        bail!("local video path is not a regular file: {}", path.display());
    }
    if metadata.len() == 0 {
        bail!("local video file is empty: {}", path.display());
    }
    drop(source_file);

    let interrupted = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&interrupted);
    ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))
        .context("could not install Ctrl-C handler")?;

    let (info, plan) = if let Some(content_type) = options.content_type.as_deref()
        && options.compatibility_mode != CompatibilityMode::Always
    {
        (
            None,
            PreparationPlan::Direct {
                content_type: content_type.to_owned(),
            },
        )
    } else {
        let info = media::inspect(&path)?;
        if let Some(duration) = info.duration
            && options.start_at > duration
        {
            bail!(
                "--start-at {:.1}s is beyond the media duration of {:.1}s",
                options.start_at,
                duration
            );
        }
        let plan = media::plan(&info, options.compatibility_mode)?;
        (Some(info), plan)
    };
    if let Some(info) = &info {
        println!(
            "Input: {} video {}x{}{}{} in {}{}.",
            info.video.codec_name,
            info.video.width,
            info.video.height,
            info.video
                .frame_rate
                .map(|fps| format!(" at {fps:.2} fps"))
                .unwrap_or_default(),
            info.audio
                .as_ref()
                .map(|audio| format!(
                    ", {} {} Hz {}-channel audio",
                    audio.codec_name, audio.sample_rate, audio.channels
                ))
                .unwrap_or_else(|| ", no audio".to_owned()),
            info.container,
            info.duration
                .map(|duration| format!(", {duration:.1}s"))
                .unwrap_or_default(),
        );
    }
    println!("Compatibility plan: {}.", plan.description());
    let incremental_target = IncrementalCastTarget {
        cast_host: options.cast_host,
        cast_port: options.cast_port,
        http_port: options.http_port,
        start_at: options.start_at,
    };

    let mut temporary = None;
    let (file, content_type, served_size) = match plan {
        PreparationPlan::Direct { content_type } => (
            File::open(&path)
                .with_context(|| format!("could not reopen local video {}", path.display()))?,
            options.content_type.unwrap_or(content_type),
            metadata.len(),
        ),
        PreparationPlan::Remux { .. } => {
            let info = info
                .as_ref()
                .ok_or_else(|| anyhow!("missing media information for remux"))?;
            let (directory, output_path) = media::temporary_mp4_path()?;
            println!("Remuxing compatible streams into MP4...");
            if let Err(error) = media::remux_to_mp4(&path, &output_path, info, &interrupted) {
                if interrupted.load(Ordering::SeqCst) {
                    return Err(error);
                }
                println!("Lossless remux was not safe ({error}); transcoding instead...");
                if options.transcode_delivery == TranscodeDelivery::Complete {
                    println!("Transcoding a complete compatibility MP4...");
                    transcode_with_progress(
                        &path,
                        &output_path,
                        info,
                        media::TranscodeTracks::all(info.audio.is_some()),
                        &interrupted,
                    )?;
                    let file = File::open(&output_path)
                        .context("could not open the transcoded compatibility MP4")?;
                    let size = file.metadata()?.len();
                    temporary = Some(directory);
                    (file, "video/mp4".to_owned(), size)
                } else {
                    drop(directory);
                    return cast_incremental_video(
                        &path,
                        info,
                        media::TranscodeTracks::all(info.audio.is_some()),
                        incremental_target,
                        interrupted,
                    );
                }
            } else {
                let file = File::open(&output_path).context("could not open the prepared MP4")?;
                let size = file.metadata()?.len();
                temporary = Some(directory);
                (file, "video/mp4".to_owned(), size)
            }
        }
        PreparationPlan::Transcode { tracks, .. } => {
            let info = info
                .as_ref()
                .ok_or_else(|| anyhow!("missing media information for transcode"))?;
            if options.transcode_delivery == TranscodeDelivery::Incremental {
                return cast_incremental_video(
                    &path,
                    info,
                    tracks,
                    incremental_target,
                    interrupted,
                );
            }
            let (directory, output_path) = media::temporary_mp4_path()?;
            println!("Transcoding a complete receiver-compatible H.264/AAC MP4...");
            transcode_with_progress(&path, &output_path, info, tracks, &interrupted)?;
            let file = File::open(&output_path).context("could not open the transcoded MP4")?;
            let size = file.metadata()?.len();
            temporary = Some(directory);
            (file, "video/mp4".to_owned(), size)
        }
    };

    let lan_ip = local_ip_for(options.cast_host, options.cast_port)?;
    let route = private_route()?;
    let server = MediaFileServer::start(
        SocketAddr::new(lan_ip, options.http_port),
        file,
        content_type.clone(),
        route,
    )?;
    let url = server.url();
    println!(
        "Preparing {} ({}, {}).",
        path.display(),
        content_type,
        human_bytes(served_size)
    );
    println!("Serving the selected video at {url}");

    let mut session = BufferedMediaSession::start(
        options.cast_host,
        options.cast_port,
        url,
        content_type,
        options.start_at,
    )?;
    let playback = monitor_playback(
        &session,
        &interrupted,
        || server.received_request(),
        || None,
    );
    let stop = session.stop();
    drop(temporary);
    let stats = server.stats();
    println!(
        "Stopped. Served {} requests ({} ranges) and {}.",
        stats.requests,
        stats.range_responses,
        human_bytes(stats.bytes_sent)
    );

    match playback {
        Ok(()) => stop.context("could not close the Cast media session"),
        Err(error) => {
            if let Err(stop_error) = stop {
                log::debug!("Cast session cleanup after playback failure failed: {stop_error:#}");
            }
            Err(error)
        }
    }
}

fn transcode_with_progress(
    input: &Path,
    output: &Path,
    info: &media::MediaInfo,
    tracks: media::TranscodeTracks,
    interrupted: &AtomicBool,
) -> Result<()> {
    let mut last_printed = -5_i32;
    media::transcode_to_mp4_with_tracks(input, output, info, tracks, interrupted, |percent| {
        let whole = percent.floor() as i32;
        if whole >= last_printed + 5 || whole == 100 {
            println!("Transcoding: {whole}%");
            last_printed = whole;
        }
    })
}

fn cast_incremental_video(
    path: &Path,
    info: &media::MediaInfo,
    tracks: media::TranscodeTracks,
    target: IncrementalCastTarget,
    interrupted: Arc<AtomicBool>,
) -> Result<()> {
    let lan_ip = local_ip_for(target.cast_host, target.cast_port)?;
    let route = private_route()?;
    println!("Preparing receiver-compatible H.264/AAC fMP4 segments...");
    let mut last_printed = -5_i32;
    let mut preparation = IncrementalHlsPreparation::start(
        path.to_owned(),
        info.clone(),
        tracks,
        SocketAddr::new(lan_ip, target.http_port),
        route,
        move |percent| {
            let whole = percent.floor() as i32;
            if whole >= last_printed + 5 || whole == 100 {
                println!("Preparing stream: {whole}%");
                last_printed = whole;
            }
        },
    )?;
    preparation.wait_until_playable(
        target.start_at,
        &interrupted,
        INCREMENTAL_PREPARATION_TIMEOUT,
    )?;
    let url = preparation.url();
    println!("Incremental stream ready at {url}");

    let mut session = BufferedMediaSession::start_fmp4_hls(
        target.cast_host,
        target.cast_port,
        url,
        target.start_at,
        info.duration,
    )?;
    let playback = monitor_playback(
        &session,
        &interrupted,
        || preparation.received_request(),
        || preparation.failure(),
    );
    let cancelled = interrupted.load(Ordering::SeqCst) || playback.is_err();
    if cancelled {
        preparation.cancel();
    }
    let stop = session.stop();
    let preparation_result = preparation.finish();
    let stats = preparation.stats();
    println!(
        "Stopped. Served {} playlists, {} init segments, {} media segments, and {}.",
        stats.playlists,
        stats.init_segments,
        stats.media_segments,
        human_bytes(stats.bytes_sent)
    );

    match playback {
        Ok(()) => {
            if !cancelled {
                preparation_result.context("incremental media preparation did not finish")?;
            }
            stop.context("could not close the Cast media session")
        }
        Err(error) => {
            if let Err(preparation_error) = preparation_result {
                log::debug!(
                    "incremental preparation cleanup after playback failure failed: {preparation_error:#}"
                );
            }
            if let Err(stop_error) = stop {
                log::debug!("Cast session cleanup after playback failure failed: {stop_error:#}");
            }
            Err(error)
        }
    }
}

fn monitor_playback(
    session: &BufferedMediaSession,
    interrupted: &AtomicBool,
    received_request: impl Fn() -> bool,
    preparation_failure: impl Fn() -> Option<String>,
) -> Result<()> {
    let started = Instant::now();
    let mut playing = false;
    loop {
        if interrupted.load(Ordering::SeqCst) {
            println!("Stopping local video cast...");
            return Ok(());
        }
        if let Some(failure) = preparation_failure() {
            bail!("incremental media preparation failed: {failure}");
        }
        if !playing && started.elapsed() >= PLAYBACK_START_TIMEOUT {
            if received_request() {
                bail!(
                    "timed out waiting for the receiver to start playback after it requested the video; the container or codecs may not be supported"
                );
            }
            bail!(
                "timed out waiting for the receiver to request the video; check the macOS firewall, LAN reachability, and client isolation"
            );
        }

        match session.recv_timeout(EVENT_POLL_INTERVAL) {
            Ok(MediaSessionEvent::Loading) => {
                println!("Receiver is loading the video...");
            }
            Ok(MediaSessionEvent::State {
                state: PlaybackState::Buffering,
                ..
            }) => {
                println!("Receiver is buffering the video...");
            }
            Ok(MediaSessionEvent::State {
                state: PlaybackState::Playing,
                current_time,
            }) => {
                if !playing {
                    let position = current_time
                        .map(|seconds| format!(" at {seconds:.1}s"))
                        .unwrap_or_default();
                    println!("Casting video{position}. Press Ctrl-C to stop.");
                    playing = true;
                } else {
                    println!("Receiver resumed playback.");
                }
            }
            Ok(MediaSessionEvent::State {
                state: PlaybackState::Paused,
                ..
            }) => {
                println!("Receiver paused playback.");
            }
            Ok(MediaSessionEvent::Ended(PlaybackEnd::Finished)) => {
                println!("Receiver finished the video.");
                return Ok(());
            }
            Ok(MediaSessionEvent::Ended(PlaybackEnd::Cancelled)) => {
                println!("Receiver stopped playback.");
                return Ok(());
            }
            Ok(MediaSessionEvent::Ended(PlaybackEnd::Interrupted)) => {
                bail!("video playback was replaced by another Cast request");
            }
            Ok(MediaSessionEvent::Failed(failure)) => {
                return Err(playback_failure(failure, received_request()));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("Cast control session ended before playback completed");
            }
        }
    }
}

fn playback_failure(failure: MediaFailure, receiver_requested_file: bool) -> anyhow::Error {
    match failure.kind {
        MediaFailureKind::Network if !receiver_requested_file => anyhow!(
            "receiver could not reach the local video server: {}; check the macOS firewall, LAN reachability, and client isolation",
            failure.detail
        ),
        MediaFailureKind::Network => anyhow!(
            "receiver encountered a network error while reading the local video: {}",
            failure.detail
        ),
        MediaFailureKind::Decode => anyhow!(
            "receiver could not decode the local video: {}; try an H.264/AAC MP4 compatible with this receiver model",
            failure.detail
        ),
        MediaFailureKind::Unsupported => anyhow!(
            "receiver does not support this video container or codec: {}; try an H.264/AAC MP4 compatible with this receiver model",
            failure.detail
        ),
        MediaFailureKind::Other => anyhow!(
            "receiver could not play the local video: {}",
            failure.detail
        ),
    }
}

fn validate_start_at(start_at: f64) -> Result<()> {
    if !start_at.is_finite() || start_at < 0.0 {
        bail!("--start-at must be a finite number greater than or equal to zero");
    }
    Ok(())
}

fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes_as_float = bytes as f64;
    if bytes_as_float >= GIB {
        format!("{:.2} GiB", bytes_as_float / GIB)
    } else if bytes_as_float >= MIB {
        format!("{:.2} MiB", bytes_as_float / MIB)
    } else if bytes_as_float >= KIB {
        format!("{:.2} KiB", bytes_as_float / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_start_position() {
        assert!(validate_start_at(0.0).is_ok());
        assert!(validate_start_at(12.5).is_ok());
        assert!(validate_start_at(-1.0).is_err());
        assert!(validate_start_at(f64::NAN).is_err());
        assert!(validate_start_at(f64::INFINITY).is_err());
    }

    #[test]
    fn formats_transfer_sizes() {
        assert_eq!(human_bytes(12), "12 bytes");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.00 GiB");
    }
}
