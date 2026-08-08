use std::{
    fs::File,
    io::{self, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::{
    cursor::{Hide, MoveToColumn, MoveToNextLine, MoveUp, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    terminal::{self, Clear, ClearType},
};

use crate::{
    cast::{
        BufferedMediaDescription, BufferedMediaSession, MediaControl, MediaFailure,
        MediaFailureKind, MediaSessionEvent, PlaybackEnd, PlaybackState, PlaybackStatus,
    },
    media::{self, CompatibilityMode, PreparationPlan},
    media_server::MediaFileServer,
    network::{local_ip_for, private_route},
    vod_hls::IncrementalHlsPreparation,
};

const PLAYBACK_START_TIMEOUT: Duration = Duration::from_secs(20);
const INCREMENTAL_PREPARATION_TIMEOUT: Duration = Duration::from_secs(120);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const UI_REFRESH_INTERVAL: Duration = Duration::from_millis(100);
const CONTROL_FEEDBACK_DURATION: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackCompletion {
    Finished,
    Stopped,
}

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
    pub interactive: bool,
}

#[derive(Clone, Copy)]
struct IncrementalCastTarget {
    cast_host: std::net::IpAddr,
    cast_port: u16,
    http_port: u16,
    start_at: f64,
    interactive: bool,
}

pub fn cast_video(options: VideoOptions) -> Result<()> {
    validate_start_at(options.start_at)?;
    let path = options
        .file
        .canonicalize()
        .with_context(|| format!("could not resolve local video {}", options.file.display()))?;
    let media_name = display_file_name(&path);
    let media_title = display_video_title(&path);
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
    let media_duration = info.as_ref().and_then(|info| info.duration);
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
        interactive: options.interactive,
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
        BufferedMediaDescription {
            title: media_title,
            duration: media_duration,
        },
        options.interactive,
    )?;
    let playback = monitor_playback(
        &session,
        &interrupted,
        || server.received_request(),
        || None,
        |_| {},
        &media_name,
        options.interactive,
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
        Ok(_) => stop.context("could not close the Cast media session"),
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
    media::transcode_to_mp4_with_tracks(input, output, info, tracks, interrupted, |progress| {
        let whole = progress.percent.floor() as i32;
        if whole >= last_printed + 5 || whole == 100 {
            println!("Transcoding: {whole}%");
            last_printed = whole;
        }
        Ok(())
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
        target.start_at,
        move |progress| {
            let whole = progress.percent.floor() as i32;
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
    let media_name = display_file_name(path);
    let media_title = display_video_title(path);

    let mut session = BufferedMediaSession::start_fmp4_hls(
        target.cast_host,
        target.cast_port,
        url,
        target.start_at,
        BufferedMediaDescription {
            title: media_title,
            duration: info.duration,
        },
        target.interactive,
    )?;
    let playback = monitor_playback(
        &session,
        &interrupted,
        || preparation.received_request(),
        || preparation.failure(),
        |position| preparation.update_playback_position(position),
        &media_name,
        target.interactive,
    );
    let cancelled = interrupted.load(Ordering::SeqCst)
        || matches!(&playback, Ok(PlaybackCompletion::Stopped) | Err(_));
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
        Ok(_) => {
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
    mut update_playback_position: impl FnMut(f64),
    media_name: &str,
    interactive: bool,
) -> Result<PlaybackCompletion> {
    let started = Instant::now();
    let mut has_played = false;
    let mut loading_announced = false;
    let mut plain_started = false;
    let mut plain_state = None;
    let mut display = PlaybackDisplay::new(started);
    let mut terminal = if interactive {
        match TerminalPlayer::enter() {
            Ok(terminal) => Some(terminal),
            Err(error) => {
                eprintln!("Interactive player unavailable; using plain output: {error:#}");
                None
            }
        }
    } else {
        None
    };
    let mut next_draw = started;

    loop {
        if interrupted.load(Ordering::SeqCst) {
            finish_terminal(&mut terminal);
            println!("Stopping local video cast...");
            return Ok(PlaybackCompletion::Stopped);
        }
        if let Some(failure) = preparation_failure() {
            bail!("incremental media preparation failed: {failure}");
        }
        if !has_played && started.elapsed() >= PLAYBACK_START_TIMEOUT {
            finish_terminal(&mut terminal);
            if received_request() {
                bail!(
                    "timed out waiting for the receiver to start playback after it requested the video; the container or codecs may not be supported"
                );
            }
            bail!(
                "timed out waiting for the receiver to request the video; check the macOS firewall, LAN reachability, and client isolation"
            );
        }

        let input = terminal
            .as_mut()
            .map(TerminalPlayer::read_actions)
            .transpose();
        match input {
            Ok(Some(actions)) => {
                let mut stop_requested = false;
                for action in actions {
                    let now = Instant::now();
                    match action {
                        PlayerAction::TogglePlayback if display.can_toggle() => {
                            session.toggle_playback()?;
                        }
                        PlayerAction::TogglePlayback => {
                            display.set_feedback("Play/pause is not available yet", now);
                        }
                        PlayerAction::SeekBy(seconds) if display.can_seek() => {
                            session.seek_by(seconds)?;
                            display.seek_optimistically(seconds, now);
                        }
                        PlayerAction::SeekBy(_) => {
                            display.set_feedback("Seek is not available yet", now);
                        }
                        PlayerAction::Stop => {
                            stop_requested = true;
                            break;
                        }
                    }
                }
                if stop_requested {
                    finish_terminal(&mut terminal);
                    println!("Stopping local video cast...");
                    return Ok(PlaybackCompletion::Stopped);
                }
            }
            Ok(None) => {}
            Err(error) => {
                disable_terminal(&mut terminal, &error);
                if let Some(status) = display.status {
                    print_plain_status(status, &mut plain_state, &mut plain_started);
                }
            }
        }

        match session.recv_timeout(EVENT_POLL_INTERVAL) {
            Ok(MediaSessionEvent::Loading) => {
                if !loading_announced {
                    if terminal.is_none() {
                        println!("Receiver is loading the video...");
                    }
                    loading_announced = true;
                }
            }
            Ok(MediaSessionEvent::Status(status)) => {
                let first_playing_status = status.state == PlaybackState::Playing && !has_played;
                if let Some(position) = status.current_time {
                    update_playback_position(position.into());
                }
                if first_playing_status {
                    has_played = true;
                }
                let now = Instant::now();
                display.update(status, now);
                if first_playing_status && let Some(player) = terminal.as_mut() {
                    let message = connected_playing_message(media_name);
                    if let Err(error) = player.announce(&message) {
                        disable_terminal(&mut terminal, &error);
                    } else {
                        next_draw = now;
                    }
                }
                if terminal.is_none() {
                    print_plain_status(status, &mut plain_state, &mut plain_started);
                }
            }
            Ok(MediaSessionEvent::ReceiverVolume(_)) => {}
            Ok(MediaSessionEvent::ControlError { control, detail }) => {
                let message = format!("{}: {detail}", control_label(control));
                if terminal.is_some() {
                    display.set_feedback(message, Instant::now());
                } else {
                    eprintln!("{message}");
                }
            }
            Ok(MediaSessionEvent::Ended(PlaybackEnd::Finished)) => {
                finish_terminal(&mut terminal);
                println!("Receiver finished the video.");
                return Ok(PlaybackCompletion::Finished);
            }
            Ok(MediaSessionEvent::Ended(PlaybackEnd::Cancelled)) => {
                finish_terminal(&mut terminal);
                println!("Receiver stopped playback.");
                return Ok(PlaybackCompletion::Stopped);
            }
            Ok(MediaSessionEvent::Ended(PlaybackEnd::Interrupted)) => {
                finish_terminal(&mut terminal);
                bail!("video playback was replaced by another Cast request");
            }
            Ok(MediaSessionEvent::Failed(failure)) => {
                finish_terminal(&mut terminal);
                return Err(playback_failure(failure, received_request()));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                finish_terminal(&mut terminal);
                bail!("Cast control session ended before playback completed");
            }
        }

        let now = Instant::now();
        if terminal.is_some() && display.status.is_some() && now >= next_draw {
            let draw_result = terminal
                .as_mut()
                .expect("interactive terminal was checked")
                .draw(&display, now);
            next_draw = now + UI_REFRESH_INTERVAL;
            if let Err(error) = draw_result {
                disable_terminal(&mut terminal, &error);
                if let Some(status) = display.status {
                    print_plain_status(status, &mut plain_state, &mut plain_started);
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PlayerAction {
    TogglePlayback,
    SeekBy(f32),
    Stop,
}

fn key_action(key: KeyEvent) -> Option<PlayerAction> {
    let pressed = key.kind == KeyEventKind::Press;
    let repeatable = pressed || key.kind == KeyEventKind::Repeat;

    if pressed
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C'))
    {
        return Some(PlayerAction::Stop);
    }

    match key.code {
        KeyCode::Esc if pressed => Some(PlayerAction::Stop),
        KeyCode::Char(' ') if pressed => Some(PlayerAction::TogglePlayback),
        KeyCode::Left if repeatable => Some(PlayerAction::SeekBy(-10.0)),
        KeyCode::Right if repeatable => Some(PlayerAction::SeekBy(10.0)),
        KeyCode::Down if repeatable => Some(PlayerAction::SeekBy(-60.0)),
        KeyCode::Up if repeatable => Some(PlayerAction::SeekBy(60.0)),
        _ => None,
    }
}

struct TerminalPlayer {
    stdout: io::Stdout,
    rendered: bool,
    active: bool,
}

impl TerminalPlayer {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode().context("could not enable raw terminal input")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error).context("could not hide the terminal cursor");
        }
        Ok(Self {
            stdout,
            rendered: false,
            active: true,
        })
    }

    fn read_actions(&mut self) -> Result<Vec<PlayerAction>> {
        let mut actions = Vec::new();
        while event::poll(Duration::ZERO).context("could not poll terminal input")? {
            if let Event::Key(key) = event::read().context("could not read terminal input")?
                && let Some(action) = key_action(key)
            {
                actions.push(action);
            }
        }
        Ok(actions)
    }

    fn draw(&mut self, display: &PlaybackDisplay, now: Instant) -> Result<()> {
        let width = terminal::size()
            .map(|(width, _)| usize::from(width))
            .unwrap_or(80)
            .saturating_sub(1)
            .max(1);
        let (progress, controls) = display.lines(now, width);

        if self.rendered {
            queue!(self.stdout, MoveUp(1))?;
        }
        queue!(self.stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        write!(self.stdout, "{progress}")?;
        queue!(
            self.stdout,
            MoveToNextLine(1),
            Clear(ClearType::CurrentLine)
        )?;
        write!(self.stdout, "{controls}")?;
        self.stdout.flush().context("could not draw video player")?;
        self.rendered = true;
        Ok(())
    }

    fn announce(&mut self, message: &str) -> Result<()> {
        self.clear_display()
            .context("could not clear the video player before announcing playback")?;
        let width = terminal::size()
            .map(|(width, _)| usize::from(width))
            .unwrap_or(80)
            .saturating_sub(1)
            .max(1);
        let message = fit_to_width(message.to_owned(), width);
        queue!(self.stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        write!(self.stdout, "{message}")?;
        queue!(self.stdout, MoveToNextLine(1), MoveToColumn(0))?;
        self.stdout
            .flush()
            .context("could not announce video playback")
    }

    fn clear_display(&mut self) -> io::Result<()> {
        if !self.rendered {
            return Ok(());
        }
        queue!(
            self.stdout,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            MoveUp(1),
            MoveToColumn(0),
            Clear(ClearType::CurrentLine)
        )?;
        self.stdout.flush()?;
        self.rendered = false;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }

        let mut first_error = self
            .clear_display()
            .err()
            .map(|error| anyhow!("could not clear the video player: {error}"));
        if let Err(error) = execute!(self.stdout, Show)
            && first_error.is_none()
        {
            first_error = Some(anyhow!("could not restore the terminal cursor: {error}"));
        }
        match terminal::disable_raw_mode() {
            Ok(()) => self.active = false,
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(anyhow!("could not restore terminal input: {error}"));
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for TerminalPlayer {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

struct PlaybackDisplay {
    status: Option<PlaybackStatus>,
    status_at: Instant,
    feedback: Option<(String, Instant)>,
}

impl PlaybackDisplay {
    fn new(now: Instant) -> Self {
        Self {
            status: None,
            status_at: now,
            feedback: None,
        }
    }

    fn update(&mut self, status: PlaybackStatus, now: Instant) {
        self.status = Some(status);
        self.status_at = now;
    }

    fn can_toggle(&self) -> bool {
        self.status.is_some_and(|status| match status.state {
            PlaybackState::Playing => status.can_pause,
            PlaybackState::Paused => true,
            PlaybackState::Buffering => false,
        })
    }

    fn can_seek(&self) -> bool {
        self.status.is_some_and(|status| {
            status.can_seek
                && status.current_time.is_some()
                && matches!(status.state, PlaybackState::Playing | PlaybackState::Paused)
        })
    }

    fn position_at(&self, now: Instant) -> Option<f32> {
        let status = self.status?;
        let mut position = status.current_time?;
        if status.state == PlaybackState::Playing && status.playback_rate > 0.0 {
            position +=
                now.saturating_duration_since(self.status_at).as_secs_f32() * status.playback_rate;
        }
        Some(clamp_position(position, status.duration))
    }

    fn seek_optimistically(&mut self, seconds: f32, now: Instant) {
        let Some(mut status) = self.status else {
            return;
        };
        let Some(current_time) = self.position_at(now) else {
            return;
        };
        status.current_time = Some(clamp_position(current_time + seconds, status.duration));
        self.status = Some(status);
        self.status_at = now;
    }

    fn set_feedback(&mut self, message: impl Into<String>, now: Instant) {
        self.feedback = Some((message.into(), now));
    }

    fn lines(&self, now: Instant, width: usize) -> (String, String) {
        let status = self.status.expect("player is drawn only after a status");
        let position = self.position_at(now);
        let progress = progress_line(status, position, width);
        let toggle = if status.state == PlaybackState::Paused {
            "play"
        } else {
            "pause"
        };
        let mut controls = format!("←/→ ±10s  ↓/↑ ±60s  Space {toggle}  Esc stop");
        if let Some((message, received_at)) = &self.feedback
            && now.saturating_duration_since(*received_at) <= CONTROL_FEEDBACK_DURATION
        {
            controls.push_str("  |  ");
            controls.push_str(message);
        }
        (fit_to_width(progress, width), fit_to_width(controls, width))
    }
}

fn clamp_position(position: f32, duration: Option<f32>) -> f32 {
    let mut position = if position.is_finite() {
        position.max(0.0)
    } else {
        0.0
    };
    if let Some(duration) = duration.filter(|value| value.is_finite() && *value >= 0.0) {
        position = position.min(duration);
    }
    position
}

fn progress_line(status: PlaybackStatus, position: Option<f32>, width: usize) -> String {
    let icon = match status.state {
        PlaybackState::Buffering => "…",
        PlaybackState::Playing => "▶",
        PlaybackState::Paused => "⏸",
    };
    let show_hours = position.is_some_and(|seconds| seconds >= 3600.0)
        || status.duration.is_some_and(|seconds| seconds >= 3600.0);
    let current = position
        .map(|seconds| format_media_time(seconds, show_hours))
        .unwrap_or_else(|| if show_hours { "-:--:--" } else { "--:--" }.to_owned());
    let total = status
        .duration
        .map(|seconds| format_media_time(seconds, show_hours))
        .unwrap_or_else(|| if show_hours { "-:--:--" } else { "--:--" }.to_owned());
    let times = format!("{current} / {total}");
    let shell_width = format!("{icon} [] {times}").chars().count();
    let bar_width = width.saturating_sub(shell_width).min(60);

    if bar_width < 8 {
        return format!("{icon} {times}");
    }

    let Some(duration) = status.duration else {
        return format!("{icon} {times}");
    };
    let bar = determinate_progress_bar(position.unwrap_or(0.0), duration, bar_width);
    format!("{icon} [{bar}] {times}")
}

fn determinate_progress_bar(position: f32, duration: f32, width: usize) -> String {
    let ratio = if duration.is_finite() && duration > 0.0 {
        f64::from(position.clamp(0.0, duration)) / f64::from(duration)
    } else {
        0.0
    };
    let filled = (ratio * width as f64).floor() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

fn format_media_time(seconds: f32, show_hours: bool) -> String {
    let total = clamp_position(seconds, None).floor() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if show_hours {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{:02}:{seconds:02}", total / 60)
    }
}

fn fit_to_width(value: String, width: usize) -> String {
    let length = value.chars().count();
    if length <= width {
        return value;
    }
    if width == 0 {
        return String::new();
    }
    let mut shortened: String = value.chars().take(width.saturating_sub(1)).collect();
    shortened.push('…');
    shortened
}

fn print_plain_status(
    status: PlaybackStatus,
    last_state: &mut Option<PlaybackState>,
    started: &mut bool,
) {
    if *last_state == Some(status.state) {
        return;
    }
    match status.state {
        PlaybackState::Buffering => println!("Receiver is buffering the video..."),
        PlaybackState::Playing if !*started => {
            let position = status
                .current_time
                .map(|seconds| format!(" at {seconds:.1}s"))
                .unwrap_or_default();
            println!("Casting video{position}. Press Ctrl-C to stop.");
            *started = true;
        }
        PlaybackState::Playing => println!("Receiver resumed playback."),
        PlaybackState::Paused => println!("Receiver paused playback."),
    }
    *last_state = Some(status.state);
}

fn control_label(control: MediaControl) -> &'static str {
    match control {
        MediaControl::PlayPause => "Play/pause failed",
        MediaControl::Seek => "Seek failed",
        MediaControl::Volume => "Volume failed",
        MediaControl::Mute => "Mute failed",
    }
}

fn finish_terminal(terminal: &mut Option<TerminalPlayer>) {
    if let Some(mut terminal) = terminal.take()
        && let Err(error) = terminal.finish()
    {
        eprintln!("Could not fully restore the terminal: {error:#}");
    }
}

fn disable_terminal(terminal: &mut Option<TerminalPlayer>, error: &anyhow::Error) {
    finish_terminal(terminal);
    eprintln!("Interactive player disabled; using plain output: {error:#}");
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

fn display_file_name(path: &Path) -> String {
    sanitized_path_name(path.file_name().unwrap_or(path.as_os_str()))
}

fn display_video_title(path: &Path) -> String {
    sanitized_path_name(
        path.file_stem()
            .or_else(|| path.file_name())
            .unwrap_or(path.as_os_str()),
    )
}

fn sanitized_path_name(name: &std::ffi::OsStr) -> String {
    let name = name.to_string_lossy();
    let sanitized: String = name
        .chars()
        .map(|character| {
            if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect();
    if sanitized.is_empty() {
        "video".to_owned()
    } else {
        sanitized
    }
}

fn connected_playing_message(media_name: &str) -> String {
    format!("Connected and playing {media_name}.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playback_status(
        state: PlaybackState,
        current_time: Option<f32>,
        duration: Option<f32>,
    ) -> PlaybackStatus {
        PlaybackStatus {
            state,
            current_time,
            duration,
            playback_rate: 1.0,
            can_pause: true,
            can_seek: true,
        }
    }

    #[test]
    fn maps_player_keys_and_repeat_behavior() {
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            Some(PlayerAction::SeekBy(-10.0))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            Some(PlayerAction::SeekBy(10.0))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Some(PlayerAction::SeekBy(-60.0))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Some(PlayerAction::SeekBy(60.0))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
            Some(PlayerAction::TogglePlayback)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(PlayerAction::Stop)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(PlayerAction::Stop)
        );

        assert_eq!(
            key_action(KeyEvent::new_with_kind(
                KeyCode::Right,
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            )),
            Some(PlayerAction::SeekBy(10.0))
        );
        assert_eq!(
            key_action(KeyEvent::new_with_kind(
                KeyCode::Char(' '),
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            )),
            None
        );
        assert_eq!(
            key_action(KeyEvent::new_with_kind(
                KeyCode::Esc,
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
            None
        );
    }

    #[test]
    fn interpolates_playing_position_and_holds_paused_position() {
        let now = Instant::now();
        let mut display = PlaybackDisplay::new(now);
        display.update(
            playback_status(PlaybackState::Playing, Some(10.0), Some(100.0)),
            now,
        );
        assert_eq!(
            display.position_at(now + Duration::from_secs(2)),
            Some(12.0)
        );

        display.seek_optimistically(-60.0, now + Duration::from_secs(2));
        assert_eq!(display.position_at(now + Duration::from_secs(3)), Some(1.0));

        display.update(
            playback_status(PlaybackState::Paused, Some(50.0), Some(100.0)),
            now,
        );
        assert_eq!(
            display.position_at(now + Duration::from_secs(5)),
            Some(50.0)
        );
    }

    #[test]
    fn renders_known_unknown_and_narrow_progress_lines() {
        let known = progress_line(
            playback_status(PlaybackState::Playing, Some(30.0), Some(120.0)),
            Some(30.0),
            50,
        );
        assert!(known.starts_with("▶ ["));
        assert!(known.contains("00:30 / 02:00"));

        let unknown = progress_line(
            playback_status(PlaybackState::Paused, Some(30.0), None),
            Some(30.0),
            50,
        );
        assert!(unknown.contains("00:30 / --:--"));
        assert!(!unknown.contains('['));

        let narrow = progress_line(
            playback_status(PlaybackState::Buffering, Some(30.0), Some(120.0)),
            Some(30.0),
            12,
        );
        assert!(!narrow.contains('['));
        assert_eq!(fit_to_width(narrow, 12).chars().count(), 12);
    }

    #[test]
    fn enables_controls_only_for_ready_supported_playback() {
        let now = Instant::now();
        let mut display = PlaybackDisplay::new(now);
        assert!(!display.can_toggle());
        assert!(!display.can_seek());

        display.update(
            playback_status(PlaybackState::Buffering, Some(10.0), Some(100.0)),
            now,
        );
        assert!(!display.can_toggle());
        assert!(!display.can_seek());

        let mut playing = playback_status(PlaybackState::Playing, Some(10.0), Some(100.0));
        playing.can_pause = false;
        display.update(playing, now);
        assert!(!display.can_toggle());
        assert!(display.can_seek());

        display.update(
            playback_status(PlaybackState::Paused, Some(10.0), Some(100.0)),
            now,
        );
        assert!(display.can_toggle());
        assert!(display.can_seek());
        assert!(display.lines(now, 80).1.contains("Space play"));
    }

    #[test]
    fn formats_media_times_with_optional_hours() {
        assert_eq!(format_media_time(65.9, false), "01:05");
        assert_eq!(format_media_time(3665.9, true), "1:01:05");
    }

    #[test]
    fn names_the_connected_video_without_terminal_control_characters() {
        assert_eq!(display_file_name(Path::new("/tmp/video.mp4")), "video.mp4");
        assert_eq!(display_video_title(Path::new("/tmp/video.mp4")), "video");
        assert_eq!(
            display_video_title(Path::new("/tmp/holiday.final.mp4")),
            "holiday.final"
        );
        assert_eq!(
            display_file_name(Path::new("/tmp/video\n.mp4")),
            "video�.mp4"
        );
        assert_eq!(display_video_title(Path::new("/tmp/video\n.mp4")), "video�");
        assert_eq!(
            connected_playing_message("video.mp4"),
            "Connected and playing video.mp4."
        );
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
}
