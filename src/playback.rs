use std::{
    fs::File,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    cast::{
        BufferedMediaDescription, BufferedMediaSession, MediaControl, MediaFailure,
        MediaFailureKind, MediaSessionEvent, PlaybackEnd, PlaybackState, PlaybackStatus,
        ReceiverVolume,
    },
    media::{self, CompatibilityMode, PreparationPlan},
    media_server::MediaFileServer,
    network::{local_ip_for, private_route},
    video::TranscodeDelivery,
    vod_hls::IncrementalHlsPreparation,
};

const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PREPARATION_TIMEOUT: Duration = Duration::from_secs(120);
const PROGRESS_EVENT_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
pub struct PlaybackOptions {
    pub path: PathBuf,
    pub receiver: IpAddr,
    pub cast_port: u16,
    pub http_port: u16,
    pub compatibility_mode: CompatibilityMode,
    pub transcode_delivery: TranscodeDelivery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparationStage {
    Inspecting,
    Remuxing,
    Transcoding,
    Ready,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlaybackEvent {
    Preparing {
        stage: PreparationStage,
        percent: Option<f64>,
    },
    Loading,
    Status(PlaybackStatus),
    ReceiverVolume(ReceiverVolume),
    ReceiverChanged(IpAddr),
    ControlError {
        control: MediaControl,
        detail: String,
    },
    Ended(PlaybackEnd),
    Stopped,
    Failed(MediaFailure),
}

struct PreparationProgressEmitter {
    events: Sender<PlaybackEvent>,
    last_sent: Option<Instant>,
}

impl PreparationProgressEmitter {
    fn new(events: Sender<PlaybackEvent>) -> Self {
        Self {
            events,
            last_sent: None,
        }
    }

    fn report(&mut self, percent: f64) {
        self.report_at(percent, Instant::now());
    }

    fn report_at(&mut self, percent: f64, now: Instant) {
        if self.last_sent.is_some_and(|last| {
            now.saturating_duration_since(last) < PROGRESS_EVENT_INTERVAL && percent < 100.0
        }) {
            return;
        }
        self.last_sent = Some(now);
        let _ = self.events.send(PlaybackEvent::Preparing {
            stage: PreparationStage::Transcoding,
            percent: Some(percent),
        });
    }
}

enum PlaybackCommand {
    Toggle,
    SeekBy(f32),
    SeekTo(f32),
    Volume,
    Mute(bool),
    SwitchReceiver(IpAddr),
    Stop,
}

#[derive(Default)]
struct VolumeMailbox {
    pending: Mutex<Option<f32>>,
}

impl VolumeMailbox {
    fn submit(&self, level: f32) -> bool {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let should_wake = pending.is_none();
        *pending = Some(level.clamp(0.0, 1.0));
        should_wake
    }

    fn take(&self) -> Option<f32> {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

pub struct PlaybackHandle {
    commands: Sender<PlaybackCommand>,
    volume: Arc<VolumeMailbox>,
    events: Receiver<PlaybackEvent>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl PlaybackHandle {
    pub fn start(options: PlaybackOptions) -> Result<Self> {
        let (commands, command_receiver) = mpsc::channel();
        let (event_sender, events) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let volume = Arc::new(VolumeMailbox::default());
        let worker_volume = Arc::clone(&volume);
        let worker = thread::Builder::new()
            .name("cast-video-playback".to_owned())
            .spawn(move || {
                if let Err(error) = run(
                    options,
                    &command_receiver,
                    &event_sender,
                    &worker_cancel,
                    &worker_volume,
                ) && !worker_cancel.load(Ordering::SeqCst)
                {
                    let _ = event_sender.send(PlaybackEvent::Failed(classify_error(&error)));
                }
            })
            .context("could not start local-video playback worker")?;
        Ok(Self {
            commands,
            volume,
            events,
            cancel,
            worker: Some(worker),
        })
    }

    pub fn drain_events(&self, limit: usize) -> Vec<PlaybackEvent> {
        drain_receiver(&self.events, limit)
    }

    pub fn toggle(&self) -> Result<()> {
        self.send(PlaybackCommand::Toggle)
    }

    pub fn seek_by(&self, seconds: f32) -> Result<()> {
        self.send(PlaybackCommand::SeekBy(seconds))
    }

    pub fn seek_to(&self, seconds: f32) -> Result<()> {
        self.send(PlaybackCommand::SeekTo(seconds))
    }

    pub fn set_volume(&self, level: f32) -> Result<()> {
        if self.volume.submit(level)
            && let Err(error) = self.send(PlaybackCommand::Volume)
        {
            let _ = self.volume.take();
            return Err(error);
        }
        Ok(())
    }

    pub fn set_muted(&self, muted: bool) -> Result<()> {
        self.send(PlaybackCommand::Mute(muted))
    }

    pub fn switch_receiver(&self, receiver: IpAddr) -> Result<()> {
        self.send(PlaybackCommand::SwitchReceiver(receiver))
    }

    pub fn stop(&mut self) -> Result<()> {
        self.cancel.store(true, Ordering::SeqCst);
        let _ = self.commands.send(PlaybackCommand::Stop);
        self.join()
    }

    fn send(&self, command: PlaybackCommand) -> Result<()> {
        self.commands
            .send(command)
            .context("playback worker is no longer running")
    }

    fn join(&mut self) -> Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|_| anyhow!("local-video playback worker panicked"))
    }
}

fn drain_receiver<T>(receiver: &Receiver<T>, limit: usize) -> Vec<T> {
    receiver.try_iter().take(limit).collect()
}

impl Drop for PlaybackHandle {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        let _ = self.commands.send(PlaybackCommand::Stop);
        if let Err(error) = self.join() {
            log::warn!("could not join local-video playback worker: {error:#}");
        }
    }
}

enum PreparedSource {
    File {
        server: MediaFileServer,
        _temporary: Option<tempfile::TempDir>,
        content_type: String,
        duration: Option<f64>,
        title: String,
    },
    Hls {
        preparation: IncrementalHlsPreparation,
        duration: Option<f64>,
        title: String,
    },
}

impl PreparedSource {
    fn start_session(
        &self,
        receiver: IpAddr,
        port: u16,
        position: f32,
        autoplay: bool,
    ) -> Result<BufferedMediaSession> {
        let description = BufferedMediaDescription {
            title: self.title().to_owned(),
            duration: self.duration(),
        };
        match self {
            Self::File {
                server,
                content_type,
                ..
            } => BufferedMediaSession::start_with_autoplay(
                receiver,
                port,
                server.url(),
                content_type.clone(),
                f64::from(position),
                description,
                autoplay,
            ),
            Self::Hls { preparation, .. } => BufferedMediaSession::start_fmp4_hls_with_autoplay(
                receiver,
                port,
                preparation.url(),
                f64::from(position),
                description,
                autoplay,
            ),
        }
    }

    fn title(&self) -> &str {
        match self {
            Self::File { title, .. } | Self::Hls { title, .. } => title,
        }
    }

    fn duration(&self) -> Option<f64> {
        match self {
            Self::File { duration, .. } | Self::Hls { duration, .. } => *duration,
        }
    }

    fn update_position(&self, position: f32) {
        if let Self::Hls { preparation, .. } = self {
            preparation.update_playback_position(f64::from(position));
        }
    }
}

fn run(
    options: PlaybackOptions,
    commands: &Receiver<PlaybackCommand>,
    events: &Sender<PlaybackEvent>,
    cancel: &Arc<AtomicBool>,
    volume: &VolumeMailbox,
) -> Result<()> {
    let source = prepare(&options, events, cancel)?;
    if cancel.load(Ordering::SeqCst) {
        return Ok(());
    }
    events.send(PlaybackEvent::Preparing {
        stage: PreparationStage::Ready,
        percent: Some(100.0),
    })?;

    let mut receiver = options.receiver;
    let mut session = source.start_session(receiver, options.cast_port, 0.0, true)?;
    let _ = events.send(PlaybackEvent::ReceiverChanged(receiver));
    let mut status: Option<(PlaybackStatus, Instant)> = None;

    loop {
        while let Ok(command) = commands.try_recv() {
            match command {
                PlaybackCommand::Toggle => session.toggle_playback()?,
                PlaybackCommand::SeekBy(seconds) => session.seek_by(seconds)?,
                PlaybackCommand::SeekTo(seconds) => session.seek_to(seconds)?,
                PlaybackCommand::Volume => {
                    if let Some(level) = volume.take() {
                        session.set_volume(level)?;
                    }
                }
                PlaybackCommand::Mute(muted) => session.set_muted(muted)?,
                PlaybackCommand::SwitchReceiver(next) if next != receiver => {
                    let position = interpolated_position(status, Instant::now());
                    let autoplay = status
                        .map(|(value, _)| value.state != PlaybackState::Paused)
                        .unwrap_or(true);
                    session.stop()?;
                    session = source.start_session(next, options.cast_port, position, autoplay)?;
                    receiver = next;
                    status = None;
                    let _ = events.send(PlaybackEvent::ReceiverChanged(receiver));
                }
                PlaybackCommand::SwitchReceiver(_) => {}
                PlaybackCommand::Stop => {
                    let _ = session.stop();
                    let _ = events.send(PlaybackEvent::Stopped);
                    return Ok(());
                }
            }
        }

        if cancel.load(Ordering::SeqCst) {
            let _ = session.stop();
            return Ok(());
        }

        match session.recv_timeout(COMMAND_POLL_INTERVAL) {
            Ok(MediaSessionEvent::Loading) => {
                let _ = events.send(PlaybackEvent::Loading);
            }
            Ok(MediaSessionEvent::Status(next)) => {
                status = Some((next, Instant::now()));
                if let Some(position) = next.current_time {
                    source.update_position(position);
                }
                let _ = events.send(PlaybackEvent::Status(next));
            }
            Ok(MediaSessionEvent::ReceiverVolume(volume)) => {
                let _ = events.send(PlaybackEvent::ReceiverVolume(volume));
            }
            Ok(MediaSessionEvent::ControlError { control, detail }) => {
                let _ = events.send(PlaybackEvent::ControlError { control, detail });
            }
            Ok(MediaSessionEvent::Ended(end)) => {
                let _ = session.stop();
                let _ = events.send(PlaybackEvent::Ended(end));
                return Ok(());
            }
            Ok(MediaSessionEvent::Failed(failure)) => {
                let _ = session.stop();
                let _ = events.send(PlaybackEvent::Failed(failure));
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                bail!("Cast session ended without a terminal playback event")
            }
        }
    }
}

fn prepare(
    options: &PlaybackOptions,
    events: &Sender<PlaybackEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<PreparedSource> {
    let _ = events.send(PlaybackEvent::Preparing {
        stage: PreparationStage::Inspecting,
        percent: None,
    });
    let path = validate_path(&options.path)?;
    let info = media::inspect(&path)?;
    let plan = media::plan(&info, options.compatibility_mode)?;
    let title = display_title(&path);
    let duration = info.duration;

    match plan {
        PreparationPlan::Direct { content_type } => {
            start_file_source(&path, None, content_type, duration, title, options)
        }
        PreparationPlan::Remux { .. } => {
            let _ = events.send(PlaybackEvent::Preparing {
                stage: PreparationStage::Remuxing,
                percent: None,
            });
            let (directory, output) = media::temporary_mp4_path()?;
            match media::remux_to_mp4(&path, &output, &info, cancel) {
                Ok(()) => start_file_source(
                    &output,
                    Some(directory),
                    "video/mp4".to_owned(),
                    duration,
                    title,
                    options,
                ),
                Err(error) if cancel.load(Ordering::SeqCst) => Err(error),
                Err(_) => {
                    drop(directory);
                    prepare_transcode(
                        &path,
                        &info,
                        media::TranscodeTracks::all(info.audio.is_some()),
                        duration,
                        title,
                        options,
                        events,
                        cancel,
                    )
                }
            }
        }
        PreparationPlan::Transcode { tracks, .. } => prepare_transcode(
            &path, &info, tracks, duration, title, options, events, cancel,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_transcode(
    path: &Path,
    info: &media::MediaInfo,
    tracks: media::TranscodeTracks,
    duration: Option<f64>,
    title: String,
    options: &PlaybackOptions,
    events: &Sender<PlaybackEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<PreparedSource> {
    let _ = events.send(PlaybackEvent::Preparing {
        stage: PreparationStage::Transcoding,
        percent: Some(0.0),
    });
    if options.transcode_delivery == TranscodeDelivery::Incremental {
        let bind = SocketAddr::new(
            local_ip_for(options.receiver, options.cast_port)?,
            options.http_port,
        );
        let mut progress_events = PreparationProgressEmitter::new(events.clone());
        let preparation = IncrementalHlsPreparation::start(
            path.to_owned(),
            info.clone(),
            tracks,
            bind,
            private_route()?,
            0.0,
            move |progress| {
                progress_events.report(progress.percent);
            },
        )?;
        preparation.wait_until_playable(0.0, cancel, PREPARATION_TIMEOUT)?;
        if cancel.load(Ordering::SeqCst) {
            preparation.cancel();
            return Err(anyhow!("media preparation was cancelled"));
        }
        Ok(PreparedSource::Hls {
            preparation,
            duration,
            title,
        })
    } else {
        let (directory, output) = media::temporary_mp4_path()?;
        let mut progress_events = PreparationProgressEmitter::new(events.clone());
        media::transcode_to_mp4_with_tracks(
            path,
            &output,
            info,
            tracks,
            cancel,
            move |progress| {
                progress_events.report(progress.percent);
                Ok(())
            },
        )?;
        start_file_source(
            &output,
            Some(directory),
            "video/mp4".to_owned(),
            duration,
            title,
            options,
        )
    }
}

fn start_file_source(
    path: &Path,
    temporary: Option<tempfile::TempDir>,
    content_type: String,
    duration: Option<f64>,
    title: String,
    options: &PlaybackOptions,
) -> Result<PreparedSource> {
    let file = File::open(path)
        .with_context(|| format!("could not open prepared video {}", path.display()))?;
    let bind = SocketAddr::new(
        local_ip_for(options.receiver, options.cast_port)?,
        options.http_port,
    );
    let server = MediaFileServer::start(bind, file, content_type.clone(), private_route()?)?;
    Ok(PreparedSource::File {
        server,
        _temporary: temporary,
        content_type,
        duration,
        title,
    })
}

fn validate_path(path: &Path) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("could not resolve local video {}", path.display()))?;
    let metadata = path
        .metadata()
        .with_context(|| format!("could not inspect local video {}", path.display()))?;
    if !metadata.is_file() {
        bail!("local video path is not a regular file: {}", path.display());
    }
    if metadata.len() == 0 {
        bail!("local video file is empty: {}", path.display());
    }
    Ok(path)
}

fn display_title(path: &Path) -> String {
    let title = path
        .file_stem()
        .or_else(|| path.file_name())
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let title: String = title
        .chars()
        .map(|character| {
            if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect();
    if title.is_empty() {
        "video".to_owned()
    } else {
        title
    }
}

fn interpolated_position(status: Option<(PlaybackStatus, Instant)>, now: Instant) -> f32 {
    let Some((status, received_at)) = status else {
        return 0.0;
    };
    let mut position = status.current_time.unwrap_or(0.0).max(0.0);
    if status.state == PlaybackState::Playing && status.playback_rate > 0.0 {
        position += now.saturating_duration_since(received_at).as_secs_f32() * status.playback_rate;
    }
    if let Some(duration) = status.duration {
        position = position.min(duration);
    }
    position
}

fn classify_error(error: &anyhow::Error) -> MediaFailure {
    let detail = format!("{error:#}");
    let lower = detail.to_ascii_lowercase();
    let kind = if lower.contains("receiver")
        || lower.contains("cast device")
        || lower.contains("connection")
        || lower.contains("network")
    {
        MediaFailureKind::Network
    } else if lower.contains("unsupported") || lower.contains("not support") {
        MediaFailureKind::Unsupported
    } else if lower.contains("decode") || lower.contains("transcod") || lower.contains("remux") {
        MediaFailureKind::Decode
    } else {
        MediaFailureKind::Other
    };
    MediaFailure { kind, detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_only_playing_positions_and_clamps_to_duration() {
        let now = Instant::now();
        let status = PlaybackStatus {
            state: PlaybackState::Playing,
            current_time: Some(8.0),
            duration: Some(10.0),
            playback_rate: 1.0,
            can_pause: true,
            can_seek: true,
        };
        assert_eq!(
            interpolated_position(Some((status, now)), now + Duration::from_secs(5)),
            10.0
        );
        let paused = PlaybackStatus {
            state: PlaybackState::Paused,
            ..status
        };
        assert_eq!(
            interpolated_position(Some((paused, now)), now + Duration::from_secs(5)),
            8.0
        );
    }

    #[test]
    fn preparation_errors_are_classified_for_queue_policy() {
        assert_eq!(
            classify_error(&anyhow!("receiver connection closed")).kind,
            MediaFailureKind::Network
        );
        assert_eq!(
            classify_error(&anyhow!("unsupported codec")).kind,
            MediaFailureKind::Unsupported
        );
        assert_eq!(
            classify_error(&anyhow!("transcode failed")).kind,
            MediaFailureKind::Decode
        );
    }

    #[test]
    fn progress_rate_and_per_tick_drain_are_bounded_under_load() {
        let (sender, receiver) = mpsc::channel();
        let mut progress = PreparationProgressEmitter::new(sender);
        let started = Instant::now();
        for index in 0_u32..1_000 {
            progress.report_at(
                f64::from(index) / 10.0,
                started + Duration::from_millis(u64::from(index)),
            );
        }
        let first_tick = drain_receiver(&receiver, 4);
        assert_eq!(first_tick.len(), 4);
        let remaining = drain_receiver(&receiver, usize::MAX);
        assert!(remaining.len() <= 7);
    }

    #[test]
    fn volume_mailbox_coalesces_pending_levels_into_one_wakeup() {
        let volume = VolumeMailbox::default();
        assert!(volume.submit(0.35));
        assert!(!volume.submit(0.4));
        assert!(!volume.submit(0.45));
        assert_eq!(volume.take(), Some(0.45));
        assert_eq!(volume.take(), None);
    }

    #[test]
    fn volume_changes_during_an_inflight_request_schedule_one_latest_followup() {
        let volume = VolumeMailbox::default();
        assert!(volume.submit(0.5));
        assert_eq!(volume.take(), Some(0.5));

        assert!(volume.submit(0.55));
        assert!(!volume.submit(0.6));
        assert!(!volume.submit(0.65));
        assert_eq!(volume.take(), Some(0.65));
        assert_eq!(volume.take(), None);
    }
}
