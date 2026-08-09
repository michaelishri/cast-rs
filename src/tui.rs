use std::{
    collections::VecDeque,
    fs,
    io::{self, IsTerminal, Stdout, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, SyncSender, TrySendError},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};

use crate::{
    cast::{MediaFailureKind, PlaybackState, PlaybackStatus, ReceiverVolume},
    discovery::{self, CastService, DeviceCapability, DiscoveryEvent, DiscoverySession},
    media::CompatibilityMode,
    playback::{PlaybackEvent, PlaybackHandle, PlaybackOptions, PreparationStage},
    video::TranscodeDelivery,
};

const TICK: Duration = Duration::from_millis(100);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 18;
const LOG_CAPACITY: usize = 500;
const VIDEO_EXTENSIONS: &[&str] = &[
    "3g2", "3gp", "asf", "avi", "f4v", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mpe", "mpeg",
    "mpg", "mts", "ogm", "ogv", "ts", "vob", "webm", "wmv",
];

#[derive(Clone, Debug)]
pub struct TuiOptions {
    pub directory: PathBuf,
    pub host: Option<IpAddr>,
    pub cast_port: u16,
    pub http_port: u16,
    pub compatibility_mode: CompatibilityMode,
    pub transcode_delivery: TranscodeDelivery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Files,
    Playlist,
    Player,
    Receiver,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PlayerKeyAction {
    Seek(f32),
    Volume(f32),
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Self::Files => Self::Playlist,
            Self::Playlist => Self::Player,
            Self::Player => Self::Receiver,
            Self::Receiver => Self::Files,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Files => Self::Receiver,
            Self::Playlist => Self::Files,
            Self::Player => Self::Playlist,
            Self::Receiver => Self::Player,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileEntry {
    path: PathBuf,
    name: String,
    directory: bool,
}

#[derive(Debug)]
struct FileNavigator {
    directory: PathBuf,
    entries: Vec<FileEntry>,
    selected: usize,
    show_all: bool,
}

impl FileNavigator {
    fn new(directory: PathBuf) -> Result<Self> {
        let directory = validate_directory(&directory)?;
        let mut navigator = Self {
            directory,
            entries: Vec::new(),
            selected: 0,
            show_all: false,
        };
        navigator.refresh()?;
        Ok(navigator)
    }

    fn refresh(&mut self) -> Result<()> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.directory)
            .with_context(|| format!("could not read directory {}", self.directory.display()))?
        {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let file_type = entry.file_type()?;
            let directory = file_type.is_dir() || (file_type.is_symlink() && entry.path().is_dir());
            let regular = file_type.is_file() || (file_type.is_symlink() && entry.path().is_file());
            if directory || (regular && (self.show_all || supported_video(&entry.path()))) {
                entries.push(FileEntry {
                    path: entry.path(),
                    name,
                    directory,
                });
            }
        }
        entries.sort_by(|left, right| {
            right
                .directory
                .cmp(&left.directory)
                .then_with(|| {
                    left.name
                        .to_ascii_lowercase()
                        .cmp(&right.name.to_ascii_lowercase())
                })
                .then_with(|| left.name.cmp(&right.name))
        });
        self.entries = entries;
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        Ok(())
    }

    fn selected(&self) -> Option<&FileEntry> {
        self.entries.get(self.selected)
    }

    fn enter_directory(&mut self, path: PathBuf) -> Result<()> {
        let path = path
            .canonicalize()
            .with_context(|| format!("could not resolve {}", path.display()))?;
        if !path.is_dir() {
            bail!("not a directory: {}", path.display());
        }
        self.directory = path;
        self.selected = 0;
        self.refresh()
    }

    fn parent(&mut self) -> Result<()> {
        let Some(parent) = self.directory.parent().map(Path::to_owned) else {
            return Ok(());
        };
        self.enter_directory(parent)
    }

    fn move_selection(&mut self, amount: isize) {
        self.selected = move_index(self.selected, self.entries.len(), amount);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaylistEntry {
    id: u64,
    path: PathBuf,
}

#[derive(Debug, Default)]
struct Playlist {
    entries: Vec<PlaylistEntry>,
    selected: usize,
    current: Option<u64>,
    next_id: u64,
}

impl Playlist {
    fn enqueue(&mut self, path: PathBuf) -> u64 {
        let id = self.allocate_id();
        self.entries.push(PlaylistEntry { id, path });
        self.selected = self.entries.len().saturating_sub(1);
        id
    }

    fn play_now(&mut self, path: PathBuf) -> u64 {
        let id = self.allocate_id();
        let index = self
            .current_index()
            .map(|index| index + 1)
            .unwrap_or_else(|| self.selected.min(self.entries.len()));
        self.entries.insert(index, PlaylistEntry { id, path });
        self.selected = index;
        self.current = Some(id);
        id
    }

    fn select_play(&mut self) -> Option<u64> {
        let id = self.entries.get(self.selected)?.id;
        self.current = Some(id);
        Some(id)
    }

    fn current_index(&self) -> Option<usize> {
        let id = self.current?;
        self.entries.iter().position(|entry| entry.id == id)
    }

    fn current_entry(&self) -> Option<&PlaylistEntry> {
        self.current_index()
            .and_then(|index| self.entries.get(index))
    }

    fn advance(&mut self) -> Option<u64> {
        let next = self.current_index()?.checked_add(1)?;
        let id = self.entries.get(next)?.id;
        self.current = Some(id);
        self.selected = next;
        Some(id)
    }

    fn previous(&mut self, elapsed: f32) -> Option<PreviousAction> {
        let current = self.current_index()?;
        if elapsed > 3.0 || current == 0 {
            return Some(PreviousAction::Restart);
        }
        let previous = current - 1;
        let id = self.entries[previous].id;
        self.current = Some(id);
        self.selected = previous;
        Some(PreviousAction::Play(id))
    }

    fn remove_selected(&mut self) -> RemoveResult {
        if self.entries.is_empty() {
            return RemoveResult::None;
        }
        let index = self.selected.min(self.entries.len() - 1);
        let removed = self.entries.remove(index);
        self.selected = index.min(self.entries.len().saturating_sub(1));
        if self.current != Some(removed.id) {
            return RemoveResult::Removed;
        }
        if let Some(next) = self.entries.get(index) {
            self.current = Some(next.id);
            self.selected = self
                .entries
                .iter()
                .position(|entry| entry.id == next.id)
                .unwrap_or(0);
            RemoveResult::ActiveAdvance(next.id)
        } else {
            self.current = None;
            RemoveResult::ActiveStopped
        }
    }

    fn reorder_selected(&mut self, amount: isize) {
        if self.entries.is_empty() {
            return;
        }
        let target = move_index(self.selected, self.entries.len(), amount);
        if target != self.selected {
            let entry = self.entries.remove(self.selected);
            self.entries.insert(target, entry);
            self.selected = target;
        }
    }

    fn allocate_id(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.next_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviousAction {
    Restart,
    Play(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoveResult {
    None,
    Removed,
    ActiveStopped,
    ActiveAdvance(u64),
}

#[derive(Clone, Copy, Debug, Default)]
struct Regions {
    files: Rect,
    playlist: Rect,
    player: Rect,
    receiver: Rect,
    progress: Rect,
    volume: Rect,
    mute: Rect,
    transport: Rect,
}

#[derive(Debug)]
struct LastClick {
    focus: Focus,
    row: usize,
    at: Instant,
}

struct App {
    options: TuiOptions,
    files: FileNavigator,
    playlist: Playlist,
    focus: Focus,
    receivers: Vec<CastService>,
    selected_receiver: Option<usize>,
    receiver_picker: bool,
    discovery: Option<DiscoverySession>,
    playback: Option<PlaybackHandle>,
    playback_status: Option<(PlaybackStatus, Instant)>,
    volume: ReceiverVolume,
    preparation: Option<(PreparationStage, Option<f64>)>,
    status: String,
    help: bool,
    logs_open: bool,
    logs: VecDeque<String>,
    log_scroll: usize,
    regions: Regions,
    last_click: Option<LastClick>,
    quit: bool,
}

impl App {
    fn new(options: TuiOptions) -> Result<Self> {
        let mut receivers = Vec::new();
        let mut selected_receiver = None;
        let discovery = if let Some(address) = options.host {
            receivers.push(CastService {
                name: address.to_string(),
                model: "Manual receiver".to_owned(),
                capability: DeviceCapability::Video,
                address,
                port: options.cast_port,
            });
            selected_receiver = Some(0);
            None
        } else {
            Some(DiscoverySession::start(Duration::from_secs(5))?)
        };
        Ok(Self {
            files: FileNavigator::new(options.directory.clone())?,
            options,
            playlist: Playlist::default(),
            focus: Focus::Files,
            receivers,
            selected_receiver,
            receiver_picker: false,
            discovery,
            playback: None,
            playback_status: None,
            volume: ReceiverVolume::default(),
            preparation: None,
            status: "Select a video and press Enter to enqueue it".to_owned(),
            help: false,
            logs_open: false,
            logs: VecDeque::new(),
            log_scroll: 0,
            regions: Regions::default(),
            last_click: None,
            quit: false,
        })
    }

    fn tick(&mut self, log_receiver: &Receiver<String>) {
        while let Ok(line) = log_receiver.try_recv() {
            if self.logs.len() == LOG_CAPACITY {
                self.logs.pop_front();
            }
            self.logs.push_back(line);
        }
        self.poll_discovery();
        self.poll_playback();
    }

    fn poll_discovery(&mut self) {
        let mut finished = false;
        if let Some(discovery) = &self.discovery {
            while let Ok(event) = discovery.try_recv() {
                match event {
                    DiscoveryEvent::Device(service) => {
                        let selected_address = self.receiver().map(|receiver| receiver.address);
                        discovery::merge_service(&mut self.receivers, service);
                        self.selected_receiver = selected_address
                            .and_then(|address| {
                                self.receivers
                                    .iter()
                                    .position(|item| item.address == address)
                            })
                            .or_else(|| (!self.receivers.is_empty()).then_some(0));
                    }
                    DiscoveryEvent::Finished => finished = true,
                    DiscoveryEvent::Failed(error) => {
                        self.status = format!("Discovery failed: {error}");
                        finished = true;
                    }
                }
            }
        }
        if finished {
            self.discovery = None;
            if self.receivers.is_empty() {
                self.status = "No Cast receivers found; press r to rescan".to_owned();
            }
        }
    }

    fn poll_playback(&mut self) {
        let mut terminal_event = None;
        if let Some(playback) = &self.playback {
            while let Ok(event) = playback.try_recv() {
                match event {
                    PlaybackEvent::Preparing { stage, percent } => {
                        self.preparation = Some((stage, percent));
                        self.status = format!("Preparing: {}", preparation_label(stage));
                    }
                    PlaybackEvent::Loading => self.status = "Loading on receiver…".to_owned(),
                    PlaybackEvent::Status(status) => {
                        self.playback_status = Some((status, Instant::now()));
                        self.preparation = None;
                        self.status = match status.state {
                            PlaybackState::Buffering => "Buffering…",
                            PlaybackState::Playing => "Playing",
                            PlaybackState::Paused => "Paused",
                        }
                        .to_owned();
                    }
                    PlaybackEvent::ReceiverVolume(volume) => self.volume = volume,
                    PlaybackEvent::ReceiverChanged(_) => {
                        self.status = "Receiver connected".to_owned()
                    }
                    PlaybackEvent::ControlError { detail, .. } => self.status = detail,
                    PlaybackEvent::Ended(_) => terminal_event = Some(QueueTerminal::Advance),
                    PlaybackEvent::Stopped => terminal_event = Some(QueueTerminal::Stopped),
                    PlaybackEvent::Failed(failure) => {
                        self.status = failure.detail;
                        terminal_event = Some(if failure.kind == MediaFailureKind::Network {
                            QueueTerminal::Pause
                        } else {
                            QueueTerminal::Advance
                        });
                    }
                }
            }
        }
        if let Some(event) = terminal_event {
            self.playback = None;
            self.playback_status = None;
            self.preparation = None;
            match event {
                QueueTerminal::Advance => {
                    if self.playlist.advance().is_some() {
                        self.start_current();
                    } else {
                        self.status = "Queue finished".to_owned();
                    }
                }
                QueueTerminal::Pause => {
                    self.status
                        .push_str(" — queue paused; choose a receiver and press Space");
                }
                QueueTerminal::Stopped => {}
            }
        }
    }

    fn receiver(&self) -> Option<&CastService> {
        self.selected_receiver
            .and_then(|index| self.receivers.get(index))
    }

    fn rescan(&mut self) {
        self.discovery = None;
        match DiscoverySession::start(Duration::from_secs(5)) {
            Ok(discovery) => {
                self.discovery = Some(discovery);
                self.status = "Scanning for Cast receivers…".to_owned();
            }
            Err(error) => self.status = format!("Could not start discovery: {error:#}"),
        }
    }

    fn start_current(&mut self) {
        let Some(path) = self
            .playlist
            .current_entry()
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        let Some(receiver) = self.receiver().map(|receiver| receiver.address) else {
            self.receiver_picker = true;
            self.focus = Focus::Receiver;
            self.status = "Choose a receiver before playing".to_owned();
            return;
        };
        self.stop_playback();
        match PlaybackHandle::start(PlaybackOptions {
            path,
            receiver,
            cast_port: self.options.cast_port,
            http_port: self.options.http_port,
            compatibility_mode: self.options.compatibility_mode,
            transcode_delivery: self.options.transcode_delivery,
        }) {
            Ok(playback) => {
                self.playback = Some(playback);
                self.playback_status = None;
            }
            Err(error) => self.status = format!("Could not start playback: {error:#}"),
        }
    }

    fn stop_playback(&mut self) {
        if let Some(mut playback) = self.playback.take() {
            let _ = playback.stop();
        }
        self.playback_status = None;
        self.preparation = None;
    }

    fn toggle_playback(&mut self) {
        if let Some(playback) = &self.playback {
            if let Err(error) = playback.toggle() {
                self.status = format!("Play/pause failed: {error:#}");
            }
        } else if self.playlist.current.is_some() || self.playlist.select_play().is_some() {
            self.start_current();
        }
    }

    fn seek_by(&mut self, seconds: f32) {
        if let Some(playback) = &self.playback
            && let Err(error) = playback.seek_by(seconds)
        {
            self.status = format!("Seek failed: {error:#}");
        }
    }

    fn seek_to_ratio(&mut self, ratio: f32) {
        if let (Some(playback), Some((status, _))) = (&self.playback, self.playback_status)
            && let Some(duration) = status.duration
        {
            let _ = playback.seek_to(duration * ratio.clamp(0.0, 1.0));
        }
    }

    fn adjust_volume(&mut self, delta: f32) {
        if let Some(playback) = &self.playback {
            let level = self.volume.level.unwrap_or(0.5) + delta;
            let _ = playback.set_volume(level.clamp(0.0, 1.0));
        }
    }

    fn select_receiver(&mut self, index: usize) {
        if index >= self.receivers.len() {
            return;
        }
        self.selected_receiver = Some(index);
        self.receiver_picker = false;
        let receiver = self.receivers[index].address;
        if let Some(playback) = &self.playback {
            if let Err(error) = playback.switch_receiver(receiver) {
                self.status = format!("Receiver transfer failed: {error:#}");
            }
        } else {
            self.status = format!("Selected {}", self.receivers[index].name);
        }
    }

    fn activate_file(&mut self, play_now: bool) {
        let Some(entry) = self.files.selected().cloned() else {
            return;
        };
        let resolved = match entry.path.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                self.status = format!("Could not resolve {}: {error}", entry.path.display());
                return;
            }
        };
        if resolved.is_dir() {
            if let Err(error) = self.files.enter_directory(resolved) {
                self.status = format!("{error:#}");
            }
        } else if resolved.is_file() {
            if play_now {
                self.playlist.play_now(resolved);
                self.start_current();
            } else {
                self.playlist.enqueue(resolved);
                self.status = "Added to playlist".to_owned();
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if self.help || self.logs_open || self.receiver_picker {
            self.handle_overlay_key(key);
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true
            }
            (KeyCode::Char('q'), _) => self.quit = true,
            (KeyCode::Tab, modifiers) if modifiers.contains(KeyModifiers::SHIFT) => {
                self.focus = self.focus.previous()
            }
            (KeyCode::BackTab, _) => self.focus = self.focus.previous(),
            (KeyCode::Tab, _) => self.focus = self.focus.next(),
            (KeyCode::Char('?'), _) => self.help = true,
            (KeyCode::Char('l'), _) => self.logs_open = true,
            (KeyCode::Char(' '), _) => self.toggle_playback(),
            (KeyCode::Char('s'), _) => {
                self.stop_playback();
                self.status = "Stopped".to_owned();
            }
            (KeyCode::Char('['), _) => self.previous(),
            (KeyCode::Char(']'), _) => {
                if self.playlist.advance().is_some() {
                    self.start_current();
                }
            }
            (KeyCode::Char('m'), _) => {
                if let Some(playback) = &self.playback {
                    let _ = playback.set_muted(!self.volume.muted.unwrap_or(false));
                }
            }
            (KeyCode::Char('+') | KeyCode::Char('='), _) => self.adjust_volume(0.05),
            (KeyCode::Char('-'), _) => self.adjust_volume(-0.05),
            _ => self.handle_focused_key(key),
        }
    }

    fn handle_focused_key(&mut self, key: KeyEvent) {
        match self.focus {
            Focus::Files => match key.code {
                KeyCode::Up => self.files.move_selection(-1),
                KeyCode::Down => self.files.move_selection(1),
                KeyCode::PageUp => self.files.move_selection(-10),
                KeyCode::PageDown => self.files.move_selection(10),
                KeyCode::Home => self.files.selected = 0,
                KeyCode::End => self.files.selected = self.files.entries.len().saturating_sub(1),
                KeyCode::Enter => self.activate_file(false),
                KeyCode::Backspace => {
                    if let Err(error) = self.files.parent() {
                        self.status = format!("{error:#}");
                    }
                }
                KeyCode::Char('p') => self.activate_file(true),
                KeyCode::Char('f') => {
                    self.files.show_all = !self.files.show_all;
                    if let Err(error) = self.files.refresh() {
                        self.status = format!("{error:#}");
                    }
                }
                _ => {}
            },
            Focus::Playlist => match (key.code, key.modifiers) {
                (KeyCode::Up, modifiers) if modifiers.contains(KeyModifiers::ALT) => {
                    self.playlist.reorder_selected(-1)
                }
                (KeyCode::Down, modifiers) if modifiers.contains(KeyModifiers::ALT) => {
                    self.playlist.reorder_selected(1)
                }
                (KeyCode::Up, _) => {
                    self.playlist.selected =
                        move_index(self.playlist.selected, self.playlist.entries.len(), -1)
                }
                (KeyCode::Down, _) => {
                    self.playlist.selected =
                        move_index(self.playlist.selected, self.playlist.entries.len(), 1)
                }
                (KeyCode::PageUp, _) => {
                    self.playlist.selected =
                        move_index(self.playlist.selected, self.playlist.entries.len(), -10)
                }
                (KeyCode::PageDown, _) => {
                    self.playlist.selected =
                        move_index(self.playlist.selected, self.playlist.entries.len(), 10)
                }
                (KeyCode::Home, _) => self.playlist.selected = 0,
                (KeyCode::End, _) => {
                    self.playlist.selected = self.playlist.entries.len().saturating_sub(1)
                }
                (KeyCode::Enter, _) => {
                    if self.playlist.select_play().is_some() {
                        self.start_current();
                    }
                }
                (KeyCode::Delete | KeyCode::Backspace, _) => self.remove_selected(),
                _ => {}
            },
            Focus::Player => match player_key_action(key.code) {
                Some(PlayerKeyAction::Seek(seconds)) => self.seek_by(seconds),
                Some(PlayerKeyAction::Volume(delta)) => self.adjust_volume(delta),
                None => {}
            },
            Focus::Receiver => match key.code {
                KeyCode::Enter => self.receiver_picker = true,
                KeyCode::Char('r') => self.rescan(),
                _ => {}
            },
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Esc) {
            self.help = false;
            self.logs_open = false;
            self.receiver_picker = false;
            return;
        }
        if self.receiver_picker {
            match key.code {
                KeyCode::Up => {
                    self.selected_receiver = Some(move_index(
                        self.selected_receiver.unwrap_or(0),
                        self.receivers.len(),
                        -1,
                    ))
                }
                KeyCode::Down => {
                    self.selected_receiver = Some(move_index(
                        self.selected_receiver.unwrap_or(0),
                        self.receivers.len(),
                        1,
                    ))
                }
                KeyCode::Enter => {
                    if let Some(index) = self.selected_receiver {
                        self.select_receiver(index);
                    }
                }
                KeyCode::Char('r') => self.rescan(),
                _ => {}
            }
        } else if self.logs_open {
            match key.code {
                KeyCode::Up => self.log_scroll = self.log_scroll.saturating_sub(1),
                KeyCode::Down => {
                    self.log_scroll = (self.log_scroll + 1).min(self.logs.len().saturating_sub(1))
                }
                KeyCode::PageUp => self.log_scroll = self.log_scroll.saturating_sub(10),
                KeyCode::PageDown => {
                    self.log_scroll = (self.log_scroll + 10).min(self.logs.len().saturating_sub(1))
                }
                _ => {}
            }
        }
    }

    fn previous(&mut self) {
        let elapsed = self.position_at(Instant::now());
        match self.playlist.previous(elapsed) {
            Some(PreviousAction::Restart) => self.seek_to_ratio(0.0),
            Some(PreviousAction::Play(_)) => self.start_current(),
            None => {}
        }
    }

    fn remove_selected(&mut self) {
        match self.playlist.remove_selected() {
            RemoveResult::ActiveAdvance(_) => self.start_current(),
            RemoveResult::ActiveStopped => self.stop_playback(),
            RemoveResult::None | RemoveResult::Removed => {}
        }
    }

    fn position_at(&self, now: Instant) -> f32 {
        let Some((status, received)) = self.playback_status else {
            return 0.0;
        };
        let mut position = status.current_time.unwrap_or(0.0);
        if status.state == PlaybackState::Playing {
            position += now.saturating_duration_since(received).as_secs_f32()
                * status.playback_rate.max(0.0);
        }
        status
            .duration
            .map_or(position, |duration| position.min(duration))
            .max(0.0)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        let point = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.mouse_click(point),
            MouseEventKind::ScrollUp => self.mouse_scroll(point, -3),
            MouseEventKind::ScrollDown => self.mouse_scroll(point, 3),
            _ => {}
        }
    }

    fn mouse_click(&mut self, point: Position) {
        if self.receiver_picker {
            let area = centered(
                Rect::new(
                    0,
                    0,
                    self.regions.player.right(),
                    self.regions.player.bottom(),
                ),
                60,
                60,
            );
            if area.contains(point) {
                let row = usize::from(point.y.saturating_sub(area.y + 1));
                self.select_receiver(row);
            }
            return;
        }
        if self.regions.progress.contains(point) {
            let ratio = f32::from(point.x.saturating_sub(self.regions.progress.x))
                / f32::from(self.regions.progress.width.max(1));
            self.seek_to_ratio(ratio);
            return;
        }
        if self.regions.transport.contains(point) {
            let offset = point.x.saturating_sub(self.regions.transport.x);
            let action = usize::from(offset) * 6 / usize::from(self.regions.transport.width.max(1));
            match action {
                0 => self.previous(),
                1 => self.seek_by(-10.0),
                2 => self.toggle_playback(),
                3 => self.seek_by(30.0),
                4 => {
                    if self.playlist.advance().is_some() {
                        self.start_current();
                    }
                }
                _ => {
                    self.stop_playback();
                    self.status = "Stopped".to_owned();
                }
            }
            return;
        }
        if self.regions.mute.contains(point) {
            if let Some(playback) = &self.playback {
                let _ = playback.set_muted(!self.volume.muted.unwrap_or(false));
            }
            return;
        }
        if self.regions.volume.contains(point) {
            let ratio = f32::from(point.x.saturating_sub(self.regions.volume.x))
                / f32::from(self.regions.volume.width.max(1));
            if let Some(playback) = &self.playback {
                let _ = playback.set_volume(ratio);
            }
            return;
        }
        if self.regions.receiver.contains(point) {
            self.focus = Focus::Receiver;
            self.receiver_picker = true;
            return;
        }
        if self.regions.files.contains(point) {
            self.focus = Focus::Files;
            let row = usize::from(point.y.saturating_sub(self.regions.files.y + 1));
            if row < self.files.entries.len() {
                let double = self.is_double_click(Focus::Files, row);
                self.files.selected = row;
                if double {
                    self.activate_file(false);
                }
            }
        } else if self.regions.playlist.contains(point) {
            self.focus = Focus::Playlist;
            let row = usize::from(point.y.saturating_sub(self.regions.playlist.y + 1));
            if row < self.playlist.entries.len() {
                let double = self.is_double_click(Focus::Playlist, row);
                self.playlist.selected = row;
                if double && self.playlist.select_play().is_some() {
                    self.start_current();
                }
            }
        } else if self.regions.player.contains(point) {
            self.focus = Focus::Player;
            self.toggle_playback();
        }
    }

    fn mouse_scroll(&mut self, point: Position, amount: isize) {
        if self.regions.files.contains(point) {
            self.files.move_selection(amount);
        } else if self.regions.playlist.contains(point) {
            self.playlist.selected =
                move_index(self.playlist.selected, self.playlist.entries.len(), amount);
        }
    }

    fn is_double_click(&mut self, focus: Focus, row: usize) -> bool {
        let now = Instant::now();
        let double = self.last_click.as_ref().is_some_and(|click| {
            click.focus == focus
                && click.row == row
                && now.saturating_duration_since(click.at) <= DOUBLE_CLICK
        });
        self.last_click = Some(LastClick {
            focus,
            row,
            at: now,
        });
        double
    }
}

#[derive(Clone, Copy)]
enum QueueTerminal {
    Advance,
    Pause,
    Stopped,
}

pub struct LogCapture {
    pub receiver: Receiver<String>,
    writer: LogWriter,
}

impl LogCapture {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::sync_channel(LOG_CAPACITY);
        Self {
            receiver,
            writer: LogWriter {
                sender,
                pending: Vec::new(),
            },
        }
    }

    pub fn writer(&self) -> LogWriter {
        self.writer.clone()
    }
}

impl Default for LogCapture {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct LogWriter {
    sender: SyncSender<String>,
    pending: Vec<u8>,
}

impl Write for LogWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buffer);
        while let Some(index) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line = self.pending.drain(..=index).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line).trim_end().to_owned();
            match self.sender.try_send(line) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    active: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("could not enable raw terminal input")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide) {
            let _ = disable_raw_mode();
            return Err(error).context("could not enter the TUI screen");
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, Show, DisableMouseCapture, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(error).context("could not initialize the TUI terminal");
            }
        };
        Ok(Self {
            terminal,
            active: true,
        })
    }

    fn restore(&mut self) -> Result<()> {
        if !claim_terminal_restore(&mut self.active) {
            return Ok(());
        }
        let mut first = disable_raw_mode().err().map(anyhow::Error::from);
        if let Err(error) = execute!(
            self.terminal.backend_mut(),
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        ) && first.is_none()
        {
            first = Some(error.into());
        }
        let _ = self.terminal.show_cursor();
        first.map_or(Ok(()), Err)
    }
}

fn claim_terminal_restore(active: &mut bool) -> bool {
    std::mem::replace(active, false)
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub fn run(options: TuiOptions, log_receiver: Receiver<String>) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("cast tui requires interactive terminal input and output");
    }
    validate_directory(&options.directory)?;
    let mut app = App::new(options)?;
    let mut terminal = TerminalGuard::enter()?;
    while !app.quit {
        app.tick(&log_receiver);
        terminal.terminal.draw(|frame| render(frame, &mut app))?;
        if event::poll(TICK).context("could not poll terminal events")? {
            match event::read().context("could not read terminal event")? {
                Event::Key(key) => app.handle_key(key),
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
            }
        }
    }
    app.stop_playback();
    terminal.restore()
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new(format!(
                "Terminal too small\n\nNeed at least {MIN_WIDTH}×{MIN_HEIGHT}; current size is {}×{}\n\nPress q to quit",
                area.width, area.height
            ))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title(" Cast TUI ")),
            area,
        );
        app.regions = Regions::default();
        return;
    }

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(9)])
        .split(area);
    let upper = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(vertical[0]);
    app.regions.files = upper[0];
    app.regions.playlist = upper[1];
    app.regions.player = vertical[1];
    render_files(frame, app, upper[0]);
    render_playlist(frame, app, upper[1]);
    render_player(frame, app, vertical[1]);

    if app.help {
        render_help(frame, area);
    } else if app.logs_open {
        render_logs(frame, app, area);
    } else if app.receiver_picker {
        render_receivers(frame, app, area);
    }
}

fn render_files(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let items = app.files.entries.iter().map(|entry| {
        ListItem::new(format!(
            "{} {}",
            if entry.directory { "▸" } else { " " },
            entry.name
        ))
    });
    let title = format!(
        " File Explorer — {}{} ",
        app.files.directory.display(),
        if app.files.show_all {
            " (all files)"
        } else {
            ""
        }
    );
    let list = List::new(items)
        .block(focus_block(title, app.focus == Focus::Files))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut state = ListState::default()
        .with_selected((!app.files.entries.is_empty()).then_some(app.files.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_playlist(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let items = app.playlist.entries.iter().map(|entry| {
        let current = app.playlist.current == Some(entry.id);
        let name = entry
            .path
            .file_name()
            .unwrap_or(entry.path.as_os_str())
            .to_string_lossy();
        ListItem::new(format!("{} {name}", if current { "▶" } else { " " })).style(if current {
            Style::default().fg(Color::Green)
        } else {
            Style::default()
        })
    });
    let list = List::new(items)
        .block(focus_block(" Playlist ", app.focus == Focus::Playlist))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut state = ListState::default()
        .with_selected((!app.playlist.entries.is_empty()).then_some(app.playlist.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_player(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let inner = focus_block(" Player ", app.focus == Focus::Player).inner(area);
    frame.render_widget(focus_block(" Player ", app.focus == Focus::Player), area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);
    let title = app.playlist.current_entry().map_or_else(
        || "No active video".to_owned(),
        |entry| {
            entry
                .path
                .file_name()
                .unwrap_or(entry.path.as_os_str())
                .to_string_lossy()
                .into_owned()
        },
    );
    frame.render_widget(
        Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD)),
        rows[0],
    );

    let position = app.position_at(Instant::now());
    let duration = app.playback_status.and_then(|(status, _)| status.duration);
    let ratio = duration
        .filter(|duration| *duration > 0.0)
        .map_or(0.0, |duration| (position / duration).clamp(0.0, 1.0));
    app.regions.progress = rows[1];
    frame.render_widget(
        Gauge::default()
            .ratio(f64::from(ratio))
            .label(format!(
                "{} / {}",
                format_time(position),
                duration.map_or_else(|| "--:--".to_owned(), format_time)
            ))
            .gauge_style(Style::default().fg(Color::Cyan)),
        rows[1],
    );

    let controls = "[ previous   ← -10s   Space play/pause   +30s →   ] next   s stop";
    app.regions.transport = rows[2];
    frame.render_widget(
        Paragraph::new(controls).alignment(Alignment::Center),
        rows[2],
    );

    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(18),
            Constraint::Min(10),
            Constraint::Length(30),
        ])
        .split(rows[3]);
    app.regions.mute = bottom[0];
    frame.render_widget(
        Paragraph::new(if app.volume.muted.unwrap_or(false) {
            "🔇 mute"
        } else {
            "🔊 mute"
        }),
        bottom[0],
    );
    app.regions.volume = bottom[1];
    frame.render_widget(
        Gauge::default()
            .ratio(f64::from(app.volume.level.unwrap_or(0.0).clamp(0.0, 1.0)))
            .label(format!(
                "Volume {:.0}%",
                app.volume.level.unwrap_or(0.0) * 100.0
            )),
        bottom[1],
    );
    frame.render_widget(Paragraph::new(app.status.as_str()), bottom[2]);
    app.regions.receiver = bottom[3];
    let receiver = app
        .receiver()
        .map_or("Choose receiver", |receiver| receiver.name.as_str());
    frame.render_widget(
        Paragraph::new(format!("▣ {receiver}"))
            .alignment(Alignment::Right)
            .style(if app.focus == Focus::Receiver {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            }),
        bottom[3],
    );

    if let Some((stage, percent)) = app.preparation {
        let text = percent.map_or_else(
            || preparation_label(stage).to_owned(),
            |value| format!("{} {:.0}%", preparation_label(stage), value),
        );
        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(Color::Yellow)),
            rows[4],
        );
    } else {
        frame.render_widget(
            Paragraph::new("? help   l logs   Tab focus   q quit")
                .style(Style::default().fg(Color::DarkGray)),
            rows[4],
        );
    }
}

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered(area, 82, 78);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(concat!(
            "Global\n  Tab/Shift-Tab focus  q/Ctrl-C quit  ? help  l logs\n",
            "  Space play/pause  s stop  [/] previous/next  m mute  +/- volume\n\n",
            "Files\n  ↑/↓ PgUp/PgDn Home/End select  Enter open/enqueue\n",
            "  Backspace parent  p play now  f show all files\n\n",
            "Playlist\n  Enter play  Delete/Backspace remove  Alt-↑/Alt-↓ reorder\n\n",
            "Player\n  ← seek -10s  → seek +30s  ↓/↑ volume -/+5%\n\n",
            "Receiver\n  Enter choose receiver  r rescan\n\n",
            "Mouse\n  Click to focus/select, double-click to activate, wheel to scroll,\n",
            "  click progress/volume gauges to set them. Escape closes overlays."
        ))
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(" Help ")),
        popup,
    );
}

fn render_logs(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let popup = centered(area, 90, 82);
    frame.render_widget(Clear, popup);
    let text = app
        .logs
        .iter()
        .skip(app.log_scroll)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Logs — Esc closes "),
        ),
        popup,
    );
    let mut scrollbar = ScrollbarState::new(app.logs.len()).position(app.log_scroll);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        popup.inner(ratatui::layout::Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut scrollbar,
    );
}

fn render_receivers(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let popup = centered(area, 60, 60);
    frame.render_widget(Clear, popup);
    let items = if app.receivers.is_empty() {
        vec![ListItem::new(if app.discovery.is_some() {
            "Scanning…"
        } else {
            "No receivers found — press r to rescan"
        })]
    } else {
        app.receivers
            .iter()
            .map(|receiver| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        receiver.name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        "  {}:{}  {}",
                        receiver.address, receiver.port, receiver.model
                    )),
                ]))
            })
            .collect()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Receivers — Enter select, r rescan, Esc close "),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::Yellow))
        .highlight_symbol("› ");
    let mut state = ListState::default().with_selected(app.selected_receiver);
    frame.render_stateful_widget(list, popup, &mut state);
}

fn focus_block<'a>(title: impl Into<Line<'a>>, focused: bool) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(if focused {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        })
}

fn centered(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn preparation_label(stage: PreparationStage) -> &'static str {
    match stage {
        PreparationStage::Inspecting => "Inspecting media",
        PreparationStage::Remuxing => "Remuxing",
        PreparationStage::Transcoding => "Transcoding",
        PreparationStage::Ready => "Ready",
    }
}

fn validate_directory(path: &Path) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("could not resolve TUI directory {}", path.display()))?;
    if !path.is_dir() {
        bail!("TUI path is not a directory: {}", path.display());
    }
    Ok(path)
}

fn supported_video(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            VIDEO_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

fn player_key_action(code: KeyCode) -> Option<PlayerKeyAction> {
    match code {
        KeyCode::Left => Some(PlayerKeyAction::Seek(-10.0)),
        KeyCode::Right => Some(PlayerKeyAction::Seek(30.0)),
        KeyCode::Down => Some(PlayerKeyAction::Volume(-0.05)),
        KeyCode::Up => Some(PlayerKeyAction::Volume(0.05)),
        _ => None,
    }
}

fn move_index(current: usize, length: usize, amount: isize) -> usize {
    if length == 0 {
        return 0;
    }
    current.saturating_add_signed(amount).min(length - 1)
}

fn format_time(seconds: f32) -> String {
    let seconds = seconds.max(0.0).floor() as u64;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            (seconds % 3600) / 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use tempfile::tempdir;

    #[test]
    fn focus_cycles_in_both_directions() {
        assert_eq!(Focus::Files.next(), Focus::Playlist);
        assert_eq!(Focus::Playlist.next(), Focus::Player);
        assert_eq!(Focus::Player.next(), Focus::Receiver);
        assert_eq!(Focus::Receiver.next(), Focus::Files);
        assert_eq!(Focus::Files.previous(), Focus::Receiver);
    }

    #[test]
    fn navigator_sorts_directories_first_and_filters_video_extensions() {
        let directory = tempdir().unwrap();
        fs::create_dir(directory.path().join("z-folder")).unwrap();
        fs::write(directory.path().join("B.MP4"), b"video").unwrap();
        fs::write(directory.path().join("a.txt"), b"text").unwrap();
        fs::write(directory.path().join(".hidden.mp4"), b"hidden").unwrap();
        let mut navigator = FileNavigator::new(directory.path().to_owned()).unwrap();
        assert_eq!(
            navigator
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["z-folder", "B.MP4"]
        );
        navigator.show_all = true;
        navigator.refresh().unwrap();
        assert_eq!(
            navigator
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["z-folder", "a.txt", "B.MP4"]
        );
    }

    #[test]
    fn playlist_allows_duplicates_and_preserves_active_id_while_reordering() {
        let mut playlist = Playlist::default();
        let first = playlist.enqueue("same.mp4".into());
        let second = playlist.enqueue("same.mp4".into());
        assert_ne!(first, second);
        playlist.current = Some(first);
        playlist.selected = 0;
        playlist.reorder_selected(1);
        assert_eq!(playlist.current, Some(first));
        assert_eq!(playlist.current_index(), Some(1));
    }

    #[test]
    fn play_now_inserts_after_current_and_previous_uses_three_second_rule() {
        let mut playlist = Playlist::default();
        let first = playlist.enqueue("one.mp4".into());
        playlist.current = Some(first);
        playlist.enqueue("three.mp4".into());
        let second = playlist.play_now("two.mp4".into());
        assert_eq!(playlist.entries[1].id, second);
        assert_eq!(playlist.previous(4.0), Some(PreviousAction::Restart));
        assert_eq!(playlist.previous(2.0), Some(PreviousAction::Play(first)));
    }

    #[test]
    fn removing_active_advances_by_stable_position() {
        let mut playlist = Playlist::default();
        let first = playlist.enqueue("one.mp4".into());
        let second = playlist.enqueue("two.mp4".into());
        playlist.current = Some(first);
        playlist.selected = 0;
        assert_eq!(
            playlist.remove_selected(),
            RemoveResult::ActiveAdvance(second)
        );
        assert_eq!(playlist.current, Some(second));

        playlist.selected = 0;
        assert_eq!(playlist.remove_selected(), RemoveResult::ActiveStopped);
        assert_eq!(playlist.current, None);
    }

    #[test]
    fn coordinate_and_time_helpers_are_bounded() {
        assert_eq!(move_index(0, 3, -10), 0);
        assert_eq!(move_index(1, 3, 10), 2);
        assert_eq!(format_time(65.9), "01:05");
        assert_eq!(format_time(3665.0), "1:01:05");
    }

    #[test]
    fn player_arrows_map_to_asymmetric_seeks_and_volume_steps() {
        assert_eq!(
            player_key_action(KeyCode::Left),
            Some(PlayerKeyAction::Seek(-10.0))
        );
        assert_eq!(
            player_key_action(KeyCode::Right),
            Some(PlayerKeyAction::Seek(30.0))
        );
        assert_eq!(
            player_key_action(KeyCode::Down),
            Some(PlayerKeyAction::Volume(-0.05))
        );
        assert_eq!(
            player_key_action(KeyCode::Up),
            Some(PlayerKeyAction::Volume(0.05))
        );
    }

    #[test]
    fn test_backend_renders_normal_minimum_resized_and_too_small_layouts() {
        let directory = tempdir().unwrap();
        fs::write(
            directory.path().join("movie.mp4"),
            b"not decoded in this test",
        )
        .unwrap();
        let options = TuiOptions {
            directory: directory.path().to_owned(),
            host: Some("192.0.2.1".parse().unwrap()),
            cast_port: 8009,
            http_port: 0,
            compatibility_mode: CompatibilityMode::Auto,
            transcode_delivery: TranscodeDelivery::Incremental,
        };
        for (width, height) in [(100, 30), (MIN_WIDTH, MIN_HEIGHT), (78, 22), (40, 10)] {
            let mut app = App::new(options.clone()).unwrap();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let content = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            if width < MIN_WIDTH || height < MIN_HEIGHT {
                assert!(content.contains("Terminal too small"));
            } else {
                assert!(content.contains("File Explorer"));
                assert!(content.contains("Playlist"));
                assert!(content.contains("Player"));
            }
        }
    }

    #[test]
    fn key_mapping_cycles_focus_and_opens_overlays() {
        let directory = tempdir().unwrap();
        let options = TuiOptions {
            directory: directory.path().to_owned(),
            host: Some("192.0.2.1".parse().unwrap()),
            cast_port: 8009,
            http_port: 0,
            compatibility_mode: CompatibilityMode::Auto,
            transcode_delivery: TranscodeDelivery::Incremental,
        };
        let mut app = App::new(options).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Playlist);
        app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.focus, Focus::Files);
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(app.help);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.help);
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        assert!(app.logs_open);
    }

    #[test]
    fn playlist_accepts_macos_backspace_and_forward_delete() {
        let directory = tempdir().unwrap();
        let options = TuiOptions {
            directory: directory.path().to_owned(),
            host: Some("192.0.2.1".parse().unwrap()),
            cast_port: 8009,
            http_port: 0,
            compatibility_mode: CompatibilityMode::Auto,
            transcode_delivery: TranscodeDelivery::Incremental,
        };
        let mut app = App::new(options).unwrap();
        app.focus = Focus::Playlist;
        app.playlist.enqueue("one.mp4".into());
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(app.playlist.entries.is_empty());
        app.playlist.enqueue("two.mp4".into());
        app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        assert!(app.playlist.entries.is_empty());
    }

    #[test]
    fn log_capture_assembles_lines_and_drops_when_bounded_channel_is_full() {
        let capture = LogCapture::new();
        let mut writer = capture.writer();
        writer.write_all(b"first part").unwrap();
        assert!(capture.receiver.try_recv().is_err());
        writer.write_all(b" finishes\nsecond\n").unwrap();
        assert_eq!(capture.receiver.try_recv().unwrap(), "first part finishes");
        assert_eq!(capture.receiver.try_recv().unwrap(), "second");
        for index in 0..=LOG_CAPACITY {
            writeln!(writer, "line {index}").unwrap();
        }
        assert_eq!(
            capture.receiver.iter().take(LOG_CAPACITY).count(),
            LOG_CAPACITY
        );
    }

    #[test]
    fn terminal_restoration_is_idempotent() {
        let mut active = true;
        assert!(claim_terminal_restore(&mut active));
        assert!(!claim_terminal_restore(&mut active));
    }
}
