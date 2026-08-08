use std::{
    net::IpAddr,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use rust_cast::{
    CastConnectionInterrupt, CastDevice, ChannelMessage,
    channels::{
        connection::ConnectionResponse,
        heartbeat::HeartbeatResponse,
        media::{
            HlsSegmentFormat, IdleReason, LoadOptions, Media, MediaDetailedErrorCode,
            MediaResponse, PlayerState, ResumeState, Status, StatusEntry, StreamType,
        },
        receiver::CastDeviceApp,
    },
};

pub use rust_cast::channels::media::HlsVideoSegmentFormat;

const BUFFERED_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(250);
const BUFFERED_SESSION_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const CAST_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const CAST_TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MEDIA_COMMAND_PAUSE: u32 = 1;
const MEDIA_COMMAND_SEEK: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Buffering,
    Playing,
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackStatus {
    pub state: PlaybackState,
    pub current_time: Option<f32>,
    pub duration: Option<f32>,
    pub playback_rate: f32,
    pub can_pause: bool,
    pub can_seek: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaControl {
    PlayPause,
    Seek,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackEnd {
    Finished,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaFailureKind {
    Network,
    Decode,
    Unsupported,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaFailure {
    pub kind: MediaFailureKind,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MediaSessionEvent {
    Loading,
    Status(PlaybackStatus),
    ControlError {
        control: MediaControl,
        detail: String,
    },
    Ended(PlaybackEnd),
    Failed(MediaFailure),
}

enum MediaSessionCommand {
    TogglePlayback,
    SeekBy(f32),
    Stop,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct BufferedPlaybackSnapshot {
    state: Option<PlaybackState>,
    current_time: Option<f32>,
    duration: Option<f32>,
    playback_rate: f32,
    supported_media_commands: u32,
}

pub struct BufferedMediaSession {
    commands: Sender<MediaSessionCommand>,
    events: Receiver<MediaSessionEvent>,
    done: Receiver<std::result::Result<(), String>>,
    interrupt: CastConnectionInterrupt,
    thread: Option<JoinHandle<()>>,
}

struct MediaLoad {
    url: String,
    content_type: String,
    live: bool,
    hls_segment_format: Option<HlsSegmentFormat>,
    hls_video_segment_format: Option<HlsVideoSegmentFormat>,
}

struct BufferedMediaLoad {
    url: String,
    content_type: String,
    start_at: f64,
    duration: Option<f64>,
    fmp4_hls: bool,
    interactive: bool,
}

impl BufferedMediaSession {
    pub fn start(
        host: IpAddr,
        port: u16,
        url: String,
        content_type: String,
        start_at: f64,
        duration: Option<f64>,
        interactive: bool,
    ) -> Result<Self> {
        Self::start_with_options(
            host,
            port,
            BufferedMediaLoad {
                url,
                content_type,
                start_at,
                duration,
                fmp4_hls: false,
                interactive,
            },
        )
    }

    pub fn start_fmp4_hls(
        host: IpAddr,
        port: u16,
        url: String,
        start_at: f64,
        duration: Option<f64>,
        interactive: bool,
    ) -> Result<Self> {
        Self::start_with_options(
            host,
            port,
            BufferedMediaLoad {
                url,
                content_type: "application/x-mpegURL".to_owned(),
                start_at,
                duration,
                fmp4_hls: true,
                interactive,
            },
        )
    }

    fn start_with_options(host: IpAddr, port: u16, media_load: BufferedMediaLoad) -> Result<Self> {
        let (command_sender, command_receiver) = mpsc::channel();
        let (event_sender, event_receiver) = mpsc::channel();
        let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
        let (done_sender, done_receiver) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("cast-buffered-cast-control".into())
            .spawn(move || {
                eprintln!("Connecting to Cast receiver at {host}:{port}...");
                let device = match CastDevice::connect_without_host_verification_timeout(
                    host.to_string(),
                    port,
                    CAST_TCP_CONNECT_TIMEOUT,
                ) {
                    Ok(device) => device,
                    Err(error) => {
                        let detail =
                            format!("could not connect to Cast device at {host}:{port}: {error}");
                        let _ = startup_sender.send(Err(detail.clone()));
                        let _ = event_sender.send(MediaSessionEvent::Failed(MediaFailure {
                            kind: MediaFailureKind::Network,
                            detail: detail.clone(),
                        }));
                        let _ = done_sender.send(Err(detail));
                        return;
                    }
                };
                let interrupt = match device.connection_interrupt() {
                    Ok(interrupt) => interrupt,
                    Err(error) => {
                        let detail = format!("could not create Cast connection interrupt: {error}");
                        let _ = startup_sender.send(Err(detail.clone()));
                        let _ = event_sender.send(MediaSessionEvent::Failed(MediaFailure {
                            kind: MediaFailureKind::Other,
                            detail: detail.clone(),
                        }));
                        let _ = done_sender.send(Err(detail));
                        return;
                    }
                };
                if startup_sender.send(Ok(interrupt)).is_err() {
                    let _ = done_sender.send(Ok(()));
                    return;
                }

                let outcome = match run_buffered_media_session(
                    &device,
                    &media_load,
                    media_load.interactive,
                    &command_receiver,
                    &event_sender,
                ) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        let detail = format!("Cast control session failed: {error:#}");
                        let _ = event_sender.send(MediaSessionEvent::Failed(MediaFailure {
                            kind: MediaFailureKind::Other,
                            detail: detail.clone(),
                        }));
                        Err(detail)
                    }
                };
                let _ = done_sender.send(outcome);
            })
            .context("could not start buffered Cast control thread")?;

        let interrupt = match startup_receiver.recv_timeout(CAST_CONNECT_TIMEOUT) {
            Ok(Ok(interrupt)) => interrupt,
            Ok(Err(detail)) => {
                let _ = thread.join();
                return Err(anyhow!(detail));
            }
            Err(RecvTimeoutError::Timeout) => {
                log::warn!("Cast connection thread did not initialize within 20 seconds");
                return Err(anyhow!(
                    "Cast receiver connection did not initialize within 20 seconds"
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                return Err(anyhow!(
                    "Cast control thread ended before initializing the connection"
                ));
            }
        };

        Ok(Self {
            commands: command_sender,
            events: event_receiver,
            done: done_receiver,
            interrupt,
            thread: Some(thread),
        })
    }

    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<MediaSessionEvent, RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }

    pub fn toggle_playback(&self) -> Result<()> {
        self.commands
            .send(MediaSessionCommand::TogglePlayback)
            .context("Cast control session ended before play/pause could be requested")
    }

    pub fn seek_by(&self, seconds: f32) -> Result<()> {
        if !seconds.is_finite() || seconds == 0.0 {
            return Err(anyhow!("seek offset must be a finite non-zero number"));
        }
        self.commands
            .send(MediaSessionCommand::SeekBy(seconds))
            .context("Cast control session ended before seek could be requested")
    }

    pub fn stop(&mut self) -> Result<()> {
        self.shutdown(true)
    }

    fn shutdown(&mut self, request_stop: bool) -> Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };

        if request_stop {
            let _ = self.commands.send(MediaSessionCommand::Stop);
        }
        let mut outcome = None;
        let finished = match self.done.recv_timeout(BUFFERED_SESSION_STOP_TIMEOUT) {
            Ok(result) => {
                outcome = Some(result);
                true
            }
            Err(RecvTimeoutError::Disconnected) => true,
            Err(RecvTimeoutError::Timeout) => false,
        };
        if !finished {
            log::debug!("interrupting buffered Cast connection during shutdown");
            if let Err(error) = self.interrupt.interrupt()
                && error.kind() != std::io::ErrorKind::NotConnected
            {
                log::debug!("could not interrupt Cast connection: {error}");
            }
        }

        thread
            .join()
            .map_err(|_| anyhow!("buffered Cast control thread panicked"))?;
        let outcome = outcome
            .or_else(|| self.done.try_recv().ok())
            .ok_or_else(|| {
                anyhow!("buffered Cast control thread ended without reporting its result")
            })?;
        outcome.map_err(anyhow::Error::msg)
    }
}

impl Drop for BufferedMediaSession {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown(true) {
            log::warn!("could not shut down buffered Cast session: {error:#}");
        }
    }
}

pub fn cast_url(host: IpAddr, port: u16, url: &str, content_type: &str, live: bool) -> Result<()> {
    cast_url_with_options(host, port, url, content_type, live, None, None)
}

pub fn cast_url_with_hls_video_format(
    host: IpAddr,
    port: u16,
    url: &str,
    content_type: &str,
    live: bool,
    format: HlsVideoSegmentFormat,
) -> Result<()> {
    let segment_format = (format == HlsVideoSegmentFormat::Fmp4).then_some(HlsSegmentFormat::Fmp4);
    cast_url_with_options(
        host,
        port,
        url,
        content_type,
        live,
        segment_format,
        Some(format),
    )
}

pub fn cast_fmp4_hls(host: IpAddr, port: u16, url: &str) -> Result<()> {
    cast_url_with_options(
        host,
        port,
        url,
        "application/x-mpegURL",
        true,
        Some(HlsSegmentFormat::Fmp4),
        Some(HlsVideoSegmentFormat::Fmp4),
    )
}

fn cast_url_with_options(
    host: IpAddr,
    port: u16,
    url: &str,
    content_type: &str,
    live: bool,
    hls_segment_format: Option<HlsSegmentFormat>,
    hls_video_segment_format: Option<HlsVideoSegmentFormat>,
) -> Result<()> {
    let media_load = MediaLoad {
        url: url.to_owned(),
        content_type: content_type.to_owned(),
        live,
        hls_segment_format,
        hls_video_segment_format,
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("cast-cast-control".into())
        .spawn(move || {
            let result = cast_url_inner(host, port, &media_load, &sender);
            if let Err(error) = result {
                let message = format!("{error:#}");
                if sender.send(Err(error)).is_err() {
                    log::warn!("Cast control connection ended: {message}");
                }
            }
        })
        .context("could not start Cast control thread")?;

    receiver
        .recv_timeout(Duration::from_secs(20))
        .context("Cast receiver did not respond within 20 seconds")?
}

fn run_buffered_media_session(
    device: &CastDevice<'_>,
    media_load: &BufferedMediaLoad,
    interactive: bool,
    commands: &Receiver<MediaSessionCommand>,
    events: &Sender<MediaSessionEvent>,
) -> Result<()> {
    device
        .connection
        .connect("receiver-0")
        .context("could not initialize the Cast receiver channel")?;
    device
        .heartbeat
        .ping()
        .context("could not initialize the Cast heartbeat")?;
    if !interactive {
        eprintln!("Launching the Default Media Receiver...");
    }
    let application = device
        .receiver
        .launch_app(&CastDeviceApp::DefaultMediaReceiver)
        .context("could not launch the Default Media Receiver")?;
    device
        .connection
        .connect(&application.transport_id)
        .context("could not connect to the receiver application")?;

    events
        .send(MediaSessionEvent::Loading)
        .map_err(|_| anyhow!("local video caller stopped waiting for the Cast receiver"))?;
    let media = buffered_media(media_load);
    let load_options = LoadOptions {
        current_time: Some(media_load.start_at),
        autoplay: true,
    };
    log::debug!(
        "loading buffered media: content_type={}, start_position={:.3}s, fmp4_hls={}",
        media_load.content_type,
        media_load.start_at,
        media_load.fmp4_hls
    );
    if !interactive {
        eprintln!("Loading the buffered media URL...");
    }
    let status = match device.media.load_with_opts(
        application.transport_id.clone(),
        application.session_id.clone(),
        &media,
        load_options,
    ) {
        Ok(status) => status,
        Err(error) => {
            if emit_buffered_load_failure(device, events)? {
                return Ok(());
            }
            return Err(error).context("Cast receiver rejected the buffered media URL");
        }
    };
    let media_session_id = status
        .entries
        .iter()
        .find(|entry| {
            entry
                .media
                .as_ref()
                .is_some_and(|loaded| loaded.content_id == media_load.url)
        })
        .or_else(|| status.entries.first())
        .map(|entry| entry.media_session_id)
        .ok_or_else(|| anyhow!("Cast LOAD response did not contain a media session"))?;
    log::debug!(
        "Cast buffered LOAD response selected media session {media_session_id}: {status:?}"
    );

    let mut snapshot = BufferedPlaybackSnapshot {
        duration: normalized_duration(media_load.duration),
        ..BufferedPlaybackSnapshot::default()
    };
    if drain_buffered_messages(device, media_session_id, &mut snapshot, events)? {
        return stop_buffered_cast_session(
            device,
            &application.transport_id,
            &application.session_id,
            media_session_id,
        );
    }
    if emit_status(status, media_session_id, &mut snapshot, events) {
        return stop_buffered_cast_session(
            device,
            &application.transport_id,
            &application.session_id,
            media_session_id,
        );
    }

    loop {
        if drain_buffered_messages(device, media_session_id, &mut snapshot, events)? {
            return stop_buffered_cast_session(
                device,
                &application.transport_id,
                &application.session_id,
                media_session_id,
            );
        }

        match commands.recv_timeout(BUFFERED_STATUS_POLL_INTERVAL) {
            Ok(MediaSessionCommand::TogglePlayback) => {
                if toggle_buffered_playback(
                    device,
                    &application.transport_id,
                    media_session_id,
                    &mut snapshot,
                    events,
                ) {
                    return stop_buffered_cast_session(
                        device,
                        &application.transport_id,
                        &application.session_id,
                        media_session_id,
                    );
                }
            }
            Ok(MediaSessionCommand::SeekBy(seconds)) => {
                if seek_buffered_playback(
                    device,
                    &application.transport_id,
                    media_session_id,
                    seconds,
                    &mut snapshot,
                    events,
                ) {
                    return stop_buffered_cast_session(
                        device,
                        &application.transport_id,
                        &application.session_id,
                        media_session_id,
                    );
                }
            }
            Ok(MediaSessionCommand::Stop) => {
                stop_buffered_cast_session(
                    device,
                    &application.transport_id,
                    &application.session_id,
                    media_session_id,
                )?;
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }

        let status = device
            .media
            .get_status(&application.transport_id, Some(media_session_id))
            .context("could not poll buffered media status")?;
        if drain_buffered_messages(device, media_session_id, &mut snapshot, events)? {
            return stop_buffered_cast_session(
                device,
                &application.transport_id,
                &application.session_id,
                media_session_id,
            );
        }
        if emit_status(status, media_session_id, &mut snapshot, events) {
            return stop_buffered_cast_session(
                device,
                &application.transport_id,
                &application.session_id,
                media_session_id,
            );
        }
    }
}

fn buffered_media(media_load: &BufferedMediaLoad) -> Media {
    Media {
        content_id: media_load.url.clone(),
        stream_type: StreamType::Buffered,
        content_type: media_load.content_type.clone(),
        hls_segment_format: media_load.fmp4_hls.then_some(HlsSegmentFormat::Fmp4),
        hls_video_segment_format: media_load.fmp4_hls.then_some(HlsVideoSegmentFormat::Fmp4),
        metadata: None,
        duration: normalized_duration(media_load.duration),
    }
}

fn normalized_duration(duration: Option<f64>) -> Option<f32> {
    duration
        .map(|duration| duration as f32)
        .filter(|duration| duration.is_finite() && *duration > 0.0)
}

fn toggle_buffered_playback(
    device: &CastDevice<'_>,
    transport_id: &str,
    media_session_id: i32,
    snapshot: &mut BufferedPlaybackSnapshot,
    events: &Sender<MediaSessionEvent>,
) -> bool {
    let result = match snapshot.state {
        Some(PlaybackState::Playing)
            if snapshot.supported_media_commands & MEDIA_COMMAND_PAUSE != 0 =>
        {
            device.media.pause(transport_id, media_session_id)
        }
        Some(PlaybackState::Paused) => device.media.play(transport_id, media_session_id),
        Some(PlaybackState::Playing) => {
            return emit_control_error(
                events,
                MediaControl::PlayPause,
                "receiver does not advertise pause support",
            );
        }
        Some(PlaybackState::Buffering) | None => {
            return emit_control_error(
                events,
                MediaControl::PlayPause,
                "play/pause is unavailable until playback is ready",
            );
        }
    };

    match result {
        Ok(entry) => emit_status_entry(entry, snapshot, events),
        Err(error) => {
            log::debug!("receiver rejected play/pause control: {error}");
            emit_control_error(
                events,
                MediaControl::PlayPause,
                &format!("receiver rejected play/pause: {error}"),
            )
        }
    }
}

fn seek_buffered_playback(
    device: &CastDevice<'_>,
    transport_id: &str,
    media_session_id: i32,
    seconds: f32,
    snapshot: &mut BufferedPlaybackSnapshot,
    events: &Sender<MediaSessionEvent>,
) -> bool {
    if snapshot.supported_media_commands & MEDIA_COMMAND_SEEK == 0 {
        return emit_control_error(
            events,
            MediaControl::Seek,
            "receiver does not advertise seek support",
        );
    }
    let Some(resume_state) = seek_resume_state(snapshot.state) else {
        return emit_control_error(
            events,
            MediaControl::Seek,
            "seek is unavailable until playback is ready",
        );
    };
    let Some(current_time) = snapshot.current_time else {
        return emit_control_error(
            events,
            MediaControl::Seek,
            "receiver has not reported the current playback position",
        );
    };
    let target = seek_target(current_time, seconds, snapshot.duration);
    if (target - current_time).abs() < f32::EPSILON {
        return false;
    }

    match device.media.seek(
        transport_id,
        media_session_id,
        Some(target),
        Some(resume_state),
    ) {
        Ok(entry) => emit_status_entry(entry, snapshot, events),
        Err(error) => {
            log::debug!("receiver rejected seek control: {error}");
            emit_control_error(
                events,
                MediaControl::Seek,
                &format!("receiver rejected seek: {error}"),
            )
        }
    }
}

fn seek_target(current_time: f32, seconds: f32, duration: Option<f32>) -> f32 {
    let mut target = (current_time + seconds).max(0.0);
    if let Some(duration) = duration.filter(|value| value.is_finite() && *value >= 0.0) {
        target = target.min(duration);
    }
    target
}

fn seek_resume_state(state: Option<PlaybackState>) -> Option<ResumeState> {
    match state {
        Some(PlaybackState::Playing) => Some(ResumeState::PlaybackStart),
        Some(PlaybackState::Paused) => Some(ResumeState::PlaybackPause),
        Some(PlaybackState::Buffering) | None => None,
    }
}

fn emit_control_error(
    events: &Sender<MediaSessionEvent>,
    control: MediaControl,
    detail: &str,
) -> bool {
    events
        .send(MediaSessionEvent::ControlError {
            control,
            detail: detail.to_owned(),
        })
        .is_err()
}

fn stop_buffered_cast_session(
    device: &CastDevice<'_>,
    transport_id: &str,
    application_session_id: &str,
    media_session_id: i32,
) -> Result<()> {
    log::debug!(
        "ending buffered Cast session: media_session={media_session_id}, application_session={application_session_id}, transport={transport_id}"
    );

    if let Err(error) = device
        .media
        .stop_without_wait(transport_id, media_session_id)
    {
        log::debug!("could not send buffered media STOP during teardown: {error}");
    } else {
        log::debug!("sent buffered media STOP for session {media_session_id}");
    }

    if let Err(error) = device.connection.disconnect(transport_id) {
        log::debug!("could not close buffered media application channel: {error}");
    } else {
        log::debug!("closed buffered media application channel {transport_id}");
    }

    let stop_result = device
        .receiver
        .stop_app(application_session_id.to_owned())
        .context("receiver did not acknowledge termination of the Default Media Receiver");

    if let Err(error) = device.connection.disconnect("receiver-0") {
        log::debug!("could not close Cast receiver channel: {error}");
    } else {
        log::debug!("closed Cast receiver channel");
    }

    stop_result?;
    log::debug!("Default Media Receiver stopped cleanly");
    Ok(())
}

fn drain_buffered_messages(
    device: &CastDevice<'_>,
    media_session_id: i32,
    snapshot: &mut BufferedPlaybackSnapshot,
    events: &Sender<MediaSessionEvent>,
) -> Result<bool> {
    let mut messages = Vec::new();
    while let Some(message) = device
        .receive_buffered()
        .context("could not parse a buffered Cast message")?
    {
        messages.push(message);
    }

    if let Some(index) = messages.iter().position(is_detailed_terminal_message) {
        let message = messages.swap_remove(index);
        return handle_buffered_message(device, message, media_session_id, snapshot, events);
    }
    for message in messages {
        if handle_buffered_message(device, message, media_session_id, snapshot, events)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_detailed_terminal_message(message: &ChannelMessage) -> bool {
    matches!(
        message,
        ChannelMessage::Media(
            MediaResponse::Error(_)
                | MediaResponse::LoadFailed(_)
                | MediaResponse::LoadCancelled(_)
                | MediaResponse::InvalidPlayerState(_)
                | MediaResponse::InvalidRequest(_)
        ) | ChannelMessage::Connection(ConnectionResponse::Close)
    )
}

fn emit_buffered_load_failure(
    device: &CastDevice<'_>,
    events: &Sender<MediaSessionEvent>,
) -> Result<bool> {
    while let Some(message) = device
        .receive_buffered()
        .context("could not parse receiver details after the media load failed")?
    {
        match message {
            ChannelMessage::Heartbeat(HeartbeatResponse::Ping) => {
                device
                    .heartbeat
                    .pong()
                    .context("could not answer Cast receiver heartbeat")?;
            }
            ChannelMessage::Media(MediaResponse::Error(error)) => {
                send_terminal_event(
                    events,
                    MediaSessionEvent::Failed(detailed_media_failure(
                        error.detailed_error_code,
                        &error.message_type,
                    )),
                );
                return Ok(true);
            }
            ChannelMessage::Media(MediaResponse::LoadFailed(error)) => {
                send_terminal_event(
                    events,
                    MediaSessionEvent::Failed(MediaFailure {
                        kind: MediaFailureKind::Other,
                        detail: format!(
                            "receiver failed to load the media (request {})",
                            error.request_id
                        ),
                    }),
                );
                return Ok(true);
            }
            message => log::debug!("receiver message retained after LOAD failure: {message:?}"),
        }
    }
    Ok(false)
}

fn handle_buffered_message(
    device: &CastDevice<'_>,
    message: ChannelMessage,
    media_session_id: i32,
    snapshot: &mut BufferedPlaybackSnapshot,
    events: &Sender<MediaSessionEvent>,
) -> Result<bool> {
    match message {
        ChannelMessage::Heartbeat(HeartbeatResponse::Ping) => {
            log::trace!("buffered receiver heartbeat PING; sending PONG");
            device
                .heartbeat
                .pong()
                .context("could not answer Cast receiver heartbeat")?;
        }
        ChannelMessage::Heartbeat(HeartbeatResponse::Pong) => {
            log::trace!("buffered receiver heartbeat PONG");
        }
        ChannelMessage::Heartbeat(message) => {
            log::debug!("unrecognized buffered receiver heartbeat: {message:?}");
        }
        ChannelMessage::Media(MediaResponse::Status(status)) => {
            return Ok(emit_status(status, media_session_id, snapshot, events));
        }
        ChannelMessage::Media(MediaResponse::Error(error)) => {
            return Ok(send_terminal_event(
                events,
                MediaSessionEvent::Failed(detailed_media_failure(
                    error.detailed_error_code,
                    &error.message_type,
                )),
            ));
        }
        ChannelMessage::Media(MediaResponse::LoadFailed(error)) => {
            return Ok(send_terminal_event(
                events,
                MediaSessionEvent::Failed(MediaFailure {
                    kind: MediaFailureKind::Other,
                    detail: format!(
                        "receiver failed to load the media (request {})",
                        error.request_id
                    ),
                }),
            ));
        }
        ChannelMessage::Media(MediaResponse::LoadCancelled(_)) => {
            return Ok(send_terminal_event(
                events,
                MediaSessionEvent::Ended(PlaybackEnd::Interrupted),
            ));
        }
        ChannelMessage::Media(MediaResponse::InvalidPlayerState(error)) => {
            return Ok(send_terminal_event(
                events,
                MediaSessionEvent::Failed(MediaFailure {
                    kind: MediaFailureKind::Other,
                    detail: format!(
                        "receiver rejected the command because the player state was invalid (request {})",
                        error.request_id
                    ),
                }),
            ));
        }
        ChannelMessage::Media(MediaResponse::InvalidRequest(error)) => {
            return Ok(send_terminal_event(
                events,
                MediaSessionEvent::Failed(MediaFailure {
                    kind: MediaFailureKind::Other,
                    detail: format!(
                        "receiver rejected the media request{}",
                        error
                            .reason
                            .as_deref()
                            .map(|reason| format!(": {reason}"))
                            .unwrap_or_default()
                    ),
                }),
            ));
        }
        ChannelMessage::Media(message) => {
            log::debug!("buffered receiver media message: {message:?}");
        }
        ChannelMessage::Connection(ConnectionResponse::Close) => {
            return Ok(send_terminal_event(
                events,
                MediaSessionEvent::Failed(MediaFailure {
                    kind: MediaFailureKind::Network,
                    detail: "receiver closed the media connection".to_owned(),
                }),
            ));
        }
        ChannelMessage::Connection(message) => {
            log::debug!("buffered receiver connection message: {message:?}");
        }
        ChannelMessage::Receiver(message) => {
            log::trace!("buffered receiver status message: {message:?}");
        }
        ChannelMessage::Raw(message) => {
            log::debug!("buffered receiver raw Cast message: {message:?}");
        }
    }
    Ok(false)
}

fn emit_status(
    status: Status,
    media_session_id: i32,
    snapshot: &mut BufferedPlaybackSnapshot,
    events: &Sender<MediaSessionEvent>,
) -> bool {
    let Some(entry) = status
        .entries
        .into_iter()
        .find(|entry| entry.media_session_id == media_session_id)
    else {
        log::trace!(
            "receiver media status {} did not contain session {media_session_id}",
            status.request_id
        );
        return false;
    };
    emit_status_entry(entry, snapshot, events)
}

fn emit_status_entry(
    entry: StatusEntry,
    snapshot: &mut BufferedPlaybackSnapshot,
    events: &Sender<MediaSessionEvent>,
) -> bool {
    log::debug!(
        "buffered receiver status: state={}, extended={:?}, idle_reason={:?}, current_time={:?}",
        entry.player_state,
        entry.extended_status,
        entry.idle_reason,
        entry.current_time
    );
    match entry.player_state {
        PlayerState::Idle => match entry.idle_reason {
            Some(IdleReason::Finished) => {
                send_terminal_event(events, MediaSessionEvent::Ended(PlaybackEnd::Finished))
            }
            Some(IdleReason::Cancelled) => {
                send_terminal_event(events, MediaSessionEvent::Ended(PlaybackEnd::Cancelled))
            }
            Some(IdleReason::Interrupted) => {
                send_terminal_event(events, MediaSessionEvent::Ended(PlaybackEnd::Interrupted))
            }
            Some(IdleReason::Error) => send_terminal_event(
                events,
                MediaSessionEvent::Failed(MediaFailure {
                    kind: MediaFailureKind::Other,
                    detail: "receiver reported a media playback error".to_owned(),
                }),
            ),
            None => false,
        },
        PlayerState::Buffering => {
            emit_playback_status(PlaybackState::Buffering, &entry, snapshot, events)
        }
        PlayerState::Playing => {
            emit_playback_status(PlaybackState::Playing, &entry, snapshot, events)
        }
        PlayerState::Paused => {
            emit_playback_status(PlaybackState::Paused, &entry, snapshot, events)
        }
    }
}

fn emit_playback_status(
    state: PlaybackState,
    entry: &StatusEntry,
    snapshot: &mut BufferedPlaybackSnapshot,
    events: &Sender<MediaSessionEvent>,
) -> bool {
    snapshot.state = Some(state);
    if let Some(current_time) = entry
        .current_time
        .filter(|value| value.is_finite() && *value >= 0.0)
    {
        snapshot.current_time = Some(current_time);
    }
    if snapshot.duration.is_none() {
        snapshot.duration = entry
            .media
            .as_ref()
            .and_then(|media| media.duration)
            .filter(|value| value.is_finite() && *value > 0.0);
    }
    snapshot.playback_rate = if entry.playback_rate.is_finite() {
        entry.playback_rate
    } else {
        0.0
    };
    snapshot.supported_media_commands = entry.supported_media_commands;

    events
        .send(MediaSessionEvent::Status(PlaybackStatus {
            state,
            current_time: snapshot.current_time,
            duration: snapshot.duration,
            playback_rate: snapshot.playback_rate,
            can_pause: snapshot.supported_media_commands & MEDIA_COMMAND_PAUSE != 0,
            can_seek: snapshot.supported_media_commands & MEDIA_COMMAND_SEEK != 0,
        }))
        .is_err()
}

fn send_terminal_event(events: &Sender<MediaSessionEvent>, event: MediaSessionEvent) -> bool {
    let _ = events.send(event);
    true
}

fn detailed_media_failure(code: MediaDetailedErrorCode, message_type: &str) -> MediaFailure {
    let kind = match code {
        MediaDetailedErrorCode::MediaNetwork
        | MediaDetailedErrorCode::NetworkUnknown
        | MediaDetailedErrorCode::SegmentNetwork
        | MediaDetailedErrorCode::DashNetwork
        | MediaDetailedErrorCode::HlsNetworkKeyLoad
        | MediaDetailedErrorCode::HlsNetworkMasterPlaylist
        | MediaDetailedErrorCode::HlsNetworkNoKeyResponse
        | MediaDetailedErrorCode::HlsNetworkPlaylist
        | MediaDetailedErrorCode::SmoothNetwork => MediaFailureKind::Network,
        MediaDetailedErrorCode::MediaDecode | MediaDetailedErrorCode::SourceBufferFailure => {
            MediaFailureKind::Decode
        }
        MediaDetailedErrorCode::MediaSrcNotSupported => MediaFailureKind::Unsupported,
        _ => MediaFailureKind::Other,
    };
    MediaFailure {
        kind,
        detail: format!("receiver media error {code:?} ({message_type})"),
    }
}

fn cast_url_inner(
    host: IpAddr,
    port: u16,
    media_load: &MediaLoad,
    ready: &mpsc::SyncSender<Result<()>>,
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
    eprintln!("Launching the Default Media Receiver...");
    let application = device
        .receiver
        .launch_app(&CastDeviceApp::DefaultMediaReceiver)
        .context("could not launch the Default Media Receiver")?;

    eprintln!("Opening the receiver media channel...");
    device
        .connection
        .connect(&application.transport_id)
        .context("could not connect to the receiver application")?;

    let media = Media {
        content_id: media_load.url.clone(),
        stream_type: if media_load.live {
            StreamType::Live
        } else {
            StreamType::Buffered
        },
        content_type: media_load.content_type.clone(),
        hls_segment_format: media_load.hls_segment_format,
        hls_video_segment_format: media_load.hls_video_segment_format,
        metadata: None,
        duration: media_load.live.then_some(-1.0),
    };

    log::debug!(
        "loading media: stream_type={}, duration={:?}, start_position={}, hls_segment_format={:?}, hls_video_segment_format={:?}",
        media.stream_type,
        media.duration,
        if media_load.live { "live-edge" } else { "0s" },
        media.hls_segment_format,
        media.hls_video_segment_format
    );

    eprintln!(
        "Loading the {} media URL...",
        if media_load.live { "live" } else { "buffered" }
    );
    let load_options = LoadOptions {
        current_time: (!media_load.live).then_some(0.0),
        autoplay: true,
    };
    let status = device
        .media
        .load_with_opts(
            application.transport_id,
            application.session_id,
            &media,
            load_options,
        )
        .context("Cast receiver rejected the media URL")?;

    log::debug!(
        "Cast LOAD response contained {} media status entries: {status:?}",
        status.entries.len()
    );
    println!("Cast receiver accepted {}", media_load.url);
    ready
        .send(Ok(()))
        .map_err(|_| anyhow!("caller stopped waiting for the Cast receiver"))?;

    monitor_receiver(&device)
}

fn monitor_receiver(device: &CastDevice<'_>) -> Result<()> {
    log::debug!("monitoring Cast receiver messages and heartbeats");
    loop {
        match device
            .receive()
            .context("could not receive the next Cast message")?
        {
            ChannelMessage::Heartbeat(HeartbeatResponse::Ping) => {
                log::trace!("receiver heartbeat PING; sending PONG");
                device
                    .heartbeat
                    .pong()
                    .context("could not answer Cast receiver heartbeat")?;
            }
            ChannelMessage::Heartbeat(HeartbeatResponse::Pong) => {
                log::trace!("receiver heartbeat PONG");
            }
            ChannelMessage::Heartbeat(message) => {
                log::debug!("unrecognized receiver heartbeat message: {message:?}");
            }
            ChannelMessage::Media(MediaResponse::Status(status)) => {
                if status.entries.is_empty() {
                    log::debug!("receiver media status has no active entries");
                }
                for entry in status.entries {
                    log::debug!(
                        "receiver media status: state={}, extended={:?}, idle_reason={:?}, current_time={:?}",
                        entry.player_state,
                        entry.extended_status,
                        entry.idle_reason,
                        entry.current_time
                    );
                }
            }
            ChannelMessage::Media(MediaResponse::Error(error)) => {
                log::error!(
                    "receiver media error: code={:?}, type={}",
                    error.detailed_error_code,
                    error.message_type
                );
            }
            ChannelMessage::Media(MediaResponse::LoadFailed(error)) => {
                log::error!(
                    "receiver failed to load the media (request_id={})",
                    error.request_id
                );
            }
            ChannelMessage::Media(MediaResponse::LoadCancelled(error)) => {
                log::warn!(
                    "receiver cancelled the media load (request_id={})",
                    error.request_id
                );
            }
            ChannelMessage::Media(message) => {
                log::debug!("receiver media message: {message:?}");
            }
            ChannelMessage::Connection(message) => {
                log::debug!("receiver connection message: {message:?}");
            }
            ChannelMessage::Receiver(message) => {
                log::trace!("receiver status message: {message:?}");
            }
            ChannelMessage::Raw(message) => {
                log::debug!("receiver raw Cast message: {message:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_entry(state: PlayerState, idle_reason: Option<IdleReason>) -> StatusEntry {
        StatusEntry {
            media_session_id: 7,
            media: None,
            playback_rate: 1.0,
            player_state: state,
            current_item_id: None,
            loading_item_id: None,
            preloaded_item_id: None,
            idle_reason,
            extended_status: None,
            current_time: Some(12.5),
            supported_media_commands: 3,
        }
    }

    #[test]
    fn emits_every_status_update_and_retains_reported_duration() {
        let (sender, receiver) = mpsc::channel();
        let mut snapshot = BufferedPlaybackSnapshot::default();
        let mut first = status_entry(PlayerState::Buffering, None);
        first.media = Some(Media {
            content_id: "video".to_owned(),
            stream_type: StreamType::Buffered,
            content_type: "video/mp4".to_owned(),
            hls_segment_format: None,
            hls_video_segment_format: None,
            metadata: None,
            duration: Some(90.0),
        });
        assert!(!emit_status_entry(first, &mut snapshot, &sender));
        assert_eq!(
            receiver.recv().unwrap(),
            MediaSessionEvent::Status(PlaybackStatus {
                state: PlaybackState::Buffering,
                current_time: Some(12.5),
                duration: Some(90.0),
                playback_rate: 1.0,
                can_pause: true,
                can_seek: true,
            })
        );

        let mut second = status_entry(PlayerState::Buffering, None);
        second.current_time = Some(13.0);
        assert!(!emit_status_entry(second, &mut snapshot, &sender));
        assert_eq!(
            receiver.recv().unwrap(),
            MediaSessionEvent::Status(PlaybackStatus {
                state: PlaybackState::Buffering,
                current_time: Some(13.0),
                duration: Some(90.0),
                playback_rate: 1.0,
                can_pause: true,
                can_seek: true,
            })
        );

        assert!(!emit_status_entry(
            status_entry(PlayerState::Playing, None),
            &mut snapshot,
            &sender,
        ));
        assert_eq!(
            receiver.recv().unwrap(),
            MediaSessionEvent::Status(PlaybackStatus {
                state: PlaybackState::Playing,
                current_time: Some(12.5),
                duration: Some(90.0),
                playback_rate: 1.0,
                can_pause: true,
                can_seek: true,
            })
        );
    }

    #[test]
    fn keeps_the_source_duration_instead_of_a_receiver_buffer_window() {
        let (sender, receiver) = mpsc::channel();
        let mut snapshot = BufferedPlaybackSnapshot {
            duration: normalized_duration(Some(600.0)),
            ..BufferedPlaybackSnapshot::default()
        };
        let mut entry = status_entry(PlayerState::Playing, None);
        entry.media = Some(Media {
            content_id: "video".to_owned(),
            stream_type: StreamType::Buffered,
            content_type: "application/x-mpegURL".to_owned(),
            hls_segment_format: Some(HlsSegmentFormat::Fmp4),
            hls_video_segment_format: Some(HlsVideoSegmentFormat::Fmp4),
            metadata: None,
            duration: Some(30.0),
        });

        assert!(!emit_status_entry(entry, &mut snapshot, &sender));
        let MediaSessionEvent::Status(status) = receiver.recv().unwrap() else {
            panic!("expected playback status");
        };
        assert_eq!(status.duration, Some(600.0));
    }

    #[test]
    fn maps_idle_reasons_to_terminal_events() {
        for (reason, expected) in [
            (
                IdleReason::Finished,
                MediaSessionEvent::Ended(PlaybackEnd::Finished),
            ),
            (
                IdleReason::Cancelled,
                MediaSessionEvent::Ended(PlaybackEnd::Cancelled),
            ),
            (
                IdleReason::Interrupted,
                MediaSessionEvent::Ended(PlaybackEnd::Interrupted),
            ),
        ] {
            let (sender, receiver) = mpsc::channel();
            let mut snapshot = BufferedPlaybackSnapshot::default();
            assert!(emit_status_entry(
                status_entry(PlayerState::Idle, Some(reason)),
                &mut snapshot,
                &sender,
            ));
            assert_eq!(receiver.recv().unwrap(), expected);
        }
    }

    #[test]
    fn describes_incremental_fmp4_hls_as_buffered_media() {
        let media = buffered_media(&BufferedMediaLoad {
            url: "http://127.0.0.1/private/index.m3u8".to_owned(),
            content_type: "application/x-mpegURL".to_owned(),
            start_at: 12.0,
            duration: Some(90.0),
            fmp4_hls: true,
            interactive: false,
        });
        assert_eq!(media.stream_type, StreamType::Buffered);
        assert_eq!(media.duration, Some(90.0));
        assert_eq!(media.hls_segment_format, Some(HlsSegmentFormat::Fmp4));
        assert_eq!(
            media.hls_video_segment_format,
            Some(HlsVideoSegmentFormat::Fmp4)
        );
    }

    #[test]
    fn relative_seek_targets_are_clamped_to_media_bounds() {
        assert_eq!(seek_target(5.0, -10.0, Some(100.0)), 0.0);
        assert_eq!(seek_target(50.0, 10.0, Some(100.0)), 60.0);
        assert_eq!(seek_target(80.0, 60.0, Some(100.0)), 100.0);
        assert_eq!(seek_target(80.0, 60.0, None), 140.0);
    }

    #[test]
    fn seeking_preserves_the_current_play_pause_state() {
        assert_eq!(
            seek_resume_state(Some(PlaybackState::Playing)),
            Some(ResumeState::PlaybackStart)
        );
        assert_eq!(
            seek_resume_state(Some(PlaybackState::Paused)),
            Some(ResumeState::PlaybackPause)
        );
        assert_eq!(seek_resume_state(Some(PlaybackState::Buffering)), None);
    }

    #[test]
    fn classifies_actionable_receiver_errors() {
        assert_eq!(
            detailed_media_failure(MediaDetailedErrorCode::MediaNetwork, "MEDIA_ERROR").kind,
            MediaFailureKind::Network
        );
        assert_eq!(
            detailed_media_failure(MediaDetailedErrorCode::MediaDecode, "MEDIA_ERROR").kind,
            MediaFailureKind::Decode
        );
        assert_eq!(
            detailed_media_failure(MediaDetailedErrorCode::MediaSrcNotSupported, "MEDIA_ERROR")
                .kind,
            MediaFailureKind::Unsupported
        );
        assert_eq!(
            detailed_media_failure(MediaDetailedErrorCode::Generic, "MEDIA_ERROR").kind,
            MediaFailureKind::Other
        );
    }

    #[test]
    fn prioritizes_detailed_errors_over_generic_status_updates() {
        let error = ChannelMessage::Media(MediaResponse::Error(
            rust_cast::channels::media::MediaError {
                detailed_error_code: MediaDetailedErrorCode::MediaSrcNotSupported,
                message_type: "MEDIA_ERROR".to_owned(),
            },
        ));
        assert!(is_detailed_terminal_message(&error));
        assert!(is_detailed_terminal_message(&ChannelMessage::Connection(
            ConnectionResponse::Close
        )));
        assert!(!is_detailed_terminal_message(&ChannelMessage::Media(
            MediaResponse::Status(Status {
                request_id: 1,
                entries: Vec::new(),
            })
        )));
    }
}
