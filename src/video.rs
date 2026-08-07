use std::{
    fs::File,
    net::SocketAddr,
    os::unix::fs::FileExt,
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
    media_server::MediaFileServer,
    network::{local_ip_for, private_route},
};

const PLAYBACK_START_TIMEOUT: Duration = Duration::from_secs(20);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MEDIA_PROBE_BYTES: usize = 4096;

pub struct VideoOptions {
    pub cast_host: std::net::IpAddr,
    pub cast_port: u16,
    pub http_port: u16,
    pub file: PathBuf,
    pub start_at: f64,
    pub content_type: Option<String>,
}

pub fn cast_video(options: VideoOptions) -> Result<()> {
    validate_start_at(options.start_at)?;
    let path = options
        .file
        .canonicalize()
        .with_context(|| format!("could not resolve local video {}", options.file.display()))?;
    let file = File::open(&path)
        .with_context(|| format!("could not open local video {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("could not inspect local video {}", path.display()))?;
    if !metadata.is_file() {
        bail!("local video path is not a regular file: {}", path.display());
    }
    if metadata.len() == 0 {
        bail!("local video file is empty: {}", path.display());
    }
    let content_type = match options.content_type {
        Some(content_type) => content_type,
        None => detect_content_type(&file, &path, metadata.len())?,
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
        human_bytes(metadata.len())
    );
    println!("Serving the selected video at {url}");

    let interrupted = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&interrupted);
    ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))
        .context("could not install Ctrl-C handler")?;

    let mut session = BufferedMediaSession::start(
        options.cast_host,
        options.cast_port,
        url,
        content_type,
        options.start_at,
    )?;
    let playback = monitor_playback(&session, &server, &interrupted);
    let stop = session.stop();
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

fn monitor_playback(
    session: &BufferedMediaSession,
    server: &MediaFileServer,
    interrupted: &AtomicBool,
) -> Result<()> {
    let started = Instant::now();
    let mut playing = false;
    loop {
        if interrupted.load(Ordering::SeqCst) {
            println!("Stopping local video cast...");
            return Ok(());
        }
        if !playing && started.elapsed() >= PLAYBACK_START_TIMEOUT {
            if server.received_request() {
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
                return Err(playback_failure(failure, server.received_request()));
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

fn detect_content_type(file: &File, path: &Path, length: u64) -> Result<String> {
    let probe_length = usize::try_from(length.min(MEDIA_PROBE_BYTES as u64)).unwrap();
    let mut probe = vec![0_u8; probe_length];
    let mut offset = 0;
    while offset < probe.len() {
        let size = file.read_at(&mut probe[offset..], offset as u64)?;
        if size == 0 {
            break;
        }
        offset += size;
    }
    probe.truncate(offset);

    if is_mp4(&probe) {
        return Ok("video/mp4".to_owned());
    }
    if is_webm(&probe) {
        return Ok("video/webm".to_owned());
    }

    bail!(
        "could not identify {} as MP4 or WebM; use --content-type to try an experimental receiver-supported format",
        path.display()
    )
}

fn is_mp4(probe: &[u8]) -> bool {
    if probe.len() < 16 || &probe[4..8] != b"ftyp" {
        return false;
    }
    let box_length = u32::from_be_bytes(probe[0..4].try_into().unwrap()) as usize;
    if box_length < 16 {
        return false;
    }
    let end = box_length.min(probe.len());
    is_mp4_brand(&probe[8..12]) || probe[16..end].chunks_exact(4).any(is_mp4_brand)
}

fn is_mp4_brand(brand: &[u8]) -> bool {
    matches!(
        brand,
        b"isom"
            | b"iso2"
            | b"iso3"
            | b"iso4"
            | b"iso5"
            | b"iso6"
            | b"iso8"
            | b"iso9"
            | b"mp41"
            | b"mp42"
            | b"mp71"
            | b"avc1"
            | b"M4V "
            | b"M4VH"
            | b"F4V "
            | b"dash"
            | b"cmfc"
            | b"cmfs"
            | b"MSNV"
    )
}

fn is_webm(probe: &[u8]) -> bool {
    probe.starts_with(&[0x1a, 0x45, 0xdf, 0xa3])
        && probe.windows(7).any(|window| window == b"\x42\x82\x84webm")
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
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn detected_type(data: &[u8], suffix: &str) -> Result<String> {
        let mut file = tempfile::Builder::new().suffix(suffix).tempfile().unwrap();
        file.write_all(data).unwrap();
        file.flush().unwrap();
        let opened = File::open(file.path()).unwrap();
        detect_content_type(&opened, file.path(), data.len() as u64)
    }

    #[test]
    fn identifies_iso_bmff_as_mp4_without_trusting_extension() {
        let data = b"\0\0\0\x18ftypisom\0\0\0\0isomiso2";
        assert_eq!(detected_type(data, ".wrong").unwrap(), "video/mp4");
    }

    #[test]
    fn identifies_webm_ebml_doctype() {
        let data = b"\x1a\x45\xdf\xa3\x9f\x42\x82\x84webm";
        assert_eq!(detected_type(data, ".webm").unwrap(), "video/webm");
    }

    #[test]
    fn rejects_unknown_and_matroska_files() {
        assert!(detected_type(b"not a media file", ".mp4").is_err());
        assert!(detected_type(b"\x1a\x45\xdf\xa3\x9f\x42\x82\x88matroska", ".mkv").is_err());
    }

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

    #[test]
    fn detection_reads_from_a_stable_open_file() {
        let mut temporary = NamedTempFile::new().unwrap();
        temporary.write_all(b"\0\0\0\x18ftypisom\0\0\0\0").unwrap();
        temporary.flush().unwrap();
        let file = File::open(temporary.path()).unwrap();
        let length = file.metadata().unwrap().len();
        std::fs::remove_file(temporary.path()).unwrap();
        assert_eq!(
            detect_content_type(&file, Path::new("removed.mp4"), length).unwrap(),
            "video/mp4"
        );
    }

    #[test]
    fn does_not_misidentify_quicktime_or_image_brands_as_mp4_video() {
        assert!(detected_type(b"\0\0\0\x14ftypqt  \0\0\0\0qt  ", ".mov").is_err());
        assert!(detected_type(b"\0\0\0\x18ftypavif\0\0\0\0avifmif1", ".avif").is_err());
    }
}
