#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};

pub(crate) const SYNTHETIC_CYCLE_SECONDS: u64 = 10;
pub(crate) const SYNTHETIC_WORKLOAD_NAME: &str = "synthetic-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct H264Level {
    pub(crate) name: &'static str,
    pub(crate) codec_parameter: &'static str,
}

impl H264Level {
    pub(crate) fn for_stream(width: u32, height: u32, fps: u32, bitrate: u64) -> Result<Self> {
        let macroblocks_per_frame =
            u64::from(width.div_ceil(16)).saturating_mul(u64::from(height.div_ceil(16)));
        let macroblocks_per_second = macroblocks_per_frame.saturating_mul(u64::from(fps));
        let levels = [
            (3_600, 108_000, 14_000_000, "3.1", "avc1.42001F"),
            (5_120, 216_000, 20_000_000, "3.2", "avc1.420020"),
            (8_192, 245_760, 20_000_000, "4.0", "avc1.420028"),
            (8_192, 245_760, 50_000_000, "4.1", "avc1.420029"),
            (8_704, 522_240, 50_000_000, "4.2", "avc1.42002A"),
            (22_080, 589_824, 135_000_000, "5.0", "avc1.420032"),
            (36_864, 983_040, 240_000_000, "5.1", "avc1.420033"),
            (36_864, 2_073_600, 240_000_000, "5.2", "avc1.420034"),
        ];
        levels
            .into_iter()
            .find(|(max_frame, max_second, max_bitrate, _, _)| {
                macroblocks_per_frame <= *max_frame
                    && macroblocks_per_second <= *max_second
                    && bitrate <= *max_bitrate
            })
            .map(|(_, _, _, name, codec_parameter)| Self {
                name,
                codec_parameter,
            })
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyntheticPhase {
    Static,
    PartialMotion,
    FullMotion,
    SceneCuts,
}

impl SyntheticPhase {
    pub(crate) const ALL: [Self; 4] = [
        Self::Static,
        Self::PartialMotion,
        Self::FullMotion,
        Self::SceneCuts,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::PartialMotion => "partial motion",
            Self::FullMotion => "full motion",
            Self::SceneCuts => "scene cuts",
        }
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Static => 0,
            Self::PartialMotion => 1,
            Self::FullMotion => 2,
            Self::SceneCuts => 3,
        }
    }
}

pub(crate) fn phase_for_frame(frame_index: u64, fps: u32) -> (SyntheticPhase, u64) {
    let cycle_frames = u64::from(fps.max(1)).saturating_mul(SYNTHETIC_CYCLE_SECONDS);
    let position = frame_index % cycle_frames;
    let phase_index = position.saturating_mul(4) / cycle_frames;
    let phase_start = phase_index.saturating_mul(cycle_frames) / 4;
    let phase = match phase_index {
        0 => SyntheticPhase::Static,
        1 => SyntheticPhase::PartialMotion,
        2 => SyntheticPhase::FullMotion,
        _ => SyntheticPhase::SceneCuts,
    };
    (phase, position.saturating_sub(phase_start))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MediaTimestamp {
    pub(crate) value: i64,
    pub(crate) timescale: i32,
    pub(crate) valid: bool,
}

impl MediaTimestamp {
    #[cfg(test)]
    pub(crate) const fn new(value: i64, timescale: i32) -> Self {
        Self {
            value,
            timescale,
            valid: true,
        }
    }

    #[cfg(test)]
    pub(crate) const fn invalid() -> Self {
        Self {
            value: 0,
            timescale: 0,
            valid: false,
        }
    }
}

#[cfg(target_os = "macos")]
impl From<screencapturekit::prelude::CMTime> for MediaTimestamp {
    fn from(value: screencapturekit::prelude::CMTime) -> Self {
        Self {
            value: value.value,
            timescale: value.timescale,
            valid: value.is_valid(),
        }
    }
}

#[derive(Default)]
pub(crate) struct MediaClock {
    origin: Mutex<Option<MediaTimestamp>>,
}

impl MediaClock {
    pub(crate) fn ticks(&self, time: MediaTimestamp, target_timescale: u64) -> Option<u64> {
        if !time.valid || time.value < 0 || time.timescale <= 0 {
            return None;
        }
        let origin = {
            let mut origin = self
                .origin
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *origin.get_or_insert(time)
        };
        let delta = i128::from(time.value)
            .checked_mul(i128::from(origin.timescale))?
            .checked_sub(i128::from(origin.value).checked_mul(i128::from(time.timescale))?)?;
        if delta < 0 {
            return Some(0);
        }
        let denominator = i128::from(time.timescale).checked_mul(i128::from(origin.timescale))?;
        let ticks = delta.checked_mul(i128::from(target_timescale))? / denominator;
        u64::try_from(ticks).ok()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct EncodedAudioFrame {
    pub(crate) timestamp: u64,
    pub(crate) data: Vec<u8>,
}

pub(crate) trait EncodedAudioSink {
    fn submit_audio(&self, frame: EncodedAudioFrame) -> Result<()>;
}

pub(crate) fn fan_out_audio<S>(outputs: &[S], frame: EncodedAudioFrame) -> Result<()>
where
    S: EncodedAudioSink,
{
    for output in outputs {
        output.submit_audio(frame.clone())?;
    }
    Ok(())
}

#[allow(dead_code)] // Linux encoder adapters implement this in CAS-25.
pub(crate) trait VideoEncoderControl {
    fn set_bitrate(&self, bitrate: u32) -> Result<()>;
    fn force_keyframe(&self) -> Result<()>;
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct VideoFrameTimings {
    pub(crate) pipeline_started_at: Instant,
    pub(crate) capture_age_micros: Option<u64>,
    pub(crate) queue_wait_micros: u64,
    pub(crate) encode_micros: u64,
    pub(crate) prepare_micros: u64,
    pub(crate) sender_lock_wait_micros: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct EncodedVideoFrame {
    pub(crate) rtp_timestamp: u32,
    pub(crate) keyframe: bool,
    /// Canonical Annex-B H.264, including parameter sets on keyframes.
    pub(crate) data: Arc<Vec<u8>>,
    pub(crate) timings: VideoFrameTimings,
    pub(crate) synthetic_phase: Option<SyntheticPhase>,
}

pub(crate) trait EncodedVideoSink {
    fn submit_video(&self, frame: EncodedVideoFrame) -> Result<()>;
}

pub(crate) fn fan_out_video<S>(outputs: &[S], frame: EncodedVideoFrame) -> Result<()>
where
    S: EncodedVideoSink,
{
    for output in outputs {
        output.submit_video(frame.clone())?;
    }
    Ok(())
}

pub(crate) trait LatestFrameBackend: Send + 'static {
    type Frame: Send + 'static;

    fn has_reference_frame(&self) -> bool;

    fn queue_started_at(&self, _frame: &Self::Frame) -> Option<Instant> {
        None
    }

    fn failure_context(&self) -> &'static str {
        "desktop encoder failed"
    }

    fn process_frame(&mut self, frame: Self::Frame, queue_wait_micros: u64) -> Result<()>;
}

pub(crate) trait LatestFrameObserver: Send + Sync + 'static {
    fn submitted(&self);
    fn replaced(&self);
    fn expired(&self);
}

#[derive(Default)]
pub(crate) struct LatestFrameMetrics {
    submitted: AtomicU64,
    replaced: AtomicU64,
    expired: AtomicU64,
}

impl LatestFrameMetrics {
    #[cfg(test)]
    fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.submitted.load(Ordering::Relaxed),
            self.replaced.load(Ordering::Relaxed),
            self.expired.load(Ordering::Relaxed),
        )
    }
}

impl LatestFrameObserver for LatestFrameMetrics {
    fn submitted(&self) {
        self.submitted.fetch_add(1, Ordering::Relaxed);
    }

    fn replaced(&self) {
        self.replaced.fetch_add(1, Ordering::Relaxed);
    }

    fn expired(&self) {
        self.expired.fetch_add(1, Ordering::Relaxed);
    }
}

struct PendingFrame<F> {
    frame: F,
    submitted_at: Instant,
}

struct LatestFrameState<F> {
    pending: Option<PendingFrame<F>>,
    stopping: bool,
}

struct LatestFrameQueue<F> {
    state: Mutex<LatestFrameState<F>>,
    available: Condvar,
    observer: Arc<dyn LatestFrameObserver>,
}

pub(crate) struct LatestFrameSubmitter<F> {
    queue: Arc<LatestFrameQueue<F>>,
}

impl<F> Clone for LatestFrameSubmitter<F> {
    fn clone(&self) -> Self {
        Self {
            queue: Arc::clone(&self.queue),
        }
    }
}

impl<F> LatestFrameSubmitter<F> {
    pub(crate) fn submit(&self, frame: F) -> Result<()> {
        let mut state = self
            .queue
            .state
            .lock()
            .map_err(|_| anyhow!("raw-frame queue lock was poisoned"))?;
        if state.stopping {
            bail!("raw-frame queue has stopped");
        }
        self.queue.observer.submitted();
        let frame = PendingFrame {
            frame,
            submitted_at: Instant::now(),
        };
        if state.pending.replace(frame).is_some() {
            self.queue.observer.replaced();
        }
        self.queue.available.notify_one();
        Ok(())
    }
}

pub(crate) struct LatestFrameWorker<F: Send + 'static> {
    queue: Arc<LatestFrameQueue<F>>,
    thread: Option<JoinHandle<()>>,
}

impl<F> LatestFrameWorker<F>
where
    F: Send + 'static,
{
    pub(crate) fn start<B>(
        mut backend: B,
        max_frame_age: Option<Duration>,
        failure: Arc<Mutex<Option<String>>>,
        observer: Arc<dyn LatestFrameObserver>,
        thread_name: &str,
    ) -> Result<(LatestFrameSubmitter<F>, Self)>
    where
        B: LatestFrameBackend<Frame = F>,
    {
        let queue = Arc::new(LatestFrameQueue {
            state: Mutex::new(LatestFrameState {
                pending: None,
                stopping: false,
            }),
            available: Condvar::new(),
            observer,
        });
        let thread_queue = Arc::clone(&queue);
        let thread = thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || {
                loop {
                    let pending = {
                        let mut state = match thread_queue.state.lock() {
                            Ok(state) => state,
                            Err(_) => {
                                store_failure(&failure, "raw-frame queue lock was poisoned");
                                break;
                            }
                        };
                        while state.pending.is_none() && !state.stopping {
                            state = match thread_queue.available.wait(state) {
                                Ok(state) => state,
                                Err(_) => {
                                    store_failure(&failure, "raw-frame queue wait was poisoned");
                                    return;
                                }
                            };
                        }
                        if state.stopping {
                            break;
                        }
                        state.pending.take().expect("pending frame was checked")
                    };

                    let queue_wait = backend
                        .queue_started_at(&pending.frame)
                        .unwrap_or(pending.submitted_at)
                        .elapsed();
                    if backend.has_reference_frame()
                        && max_frame_age.is_some_and(|deadline| queue_wait > deadline)
                    {
                        thread_queue.observer.expired();
                        log::trace!(
                            "dropped raw frame after {:.1} ms queue wait",
                            queue_wait.as_secs_f64() * 1_000.0
                        );
                        continue;
                    }
                    if let Err(error) = backend.process_frame(
                        pending.frame,
                        u64::try_from(queue_wait.as_micros()).unwrap_or(u64::MAX),
                    ) {
                        store_failure(
                            &failure,
                            &format!("{}: {error:#}", backend.failure_context()),
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
            .with_context(|| format!("could not start {thread_name} worker"))?;
        Ok((
            LatestFrameSubmitter {
                queue: Arc::clone(&queue),
            },
            Self {
                queue,
                thread: Some(thread),
            },
        ))
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        self.finish()
    }

    fn finish(&mut self) -> Result<()> {
        {
            let mut state = self
                .queue
                .state
                .lock()
                .map_err(|_| anyhow!("raw-frame queue lock was poisoned during shutdown"))?;
            state.stopping = true;
            state.pending.take();
            self.queue.available.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("desktop encoder worker panicked"))?;
        }
        Ok(())
    }
}

impl<F: Send + 'static> Drop for LatestFrameWorker<F> {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            log::warn!("could not stop desktop encoder worker: {error:#}");
        }
    }
}

fn store_failure(failure: &Mutex<Option<String>>, message: &str) {
    match failure.lock() {
        Ok(mut failure) => {
            if failure.is_none() {
                *failure = Some(message.to_owned());
            }
        }
        Err(_) => log::error!("could not store desktop worker failure: {message}"),
    }
}

const HEALTHY_RATE_WINDOWS_BEFORE_INCREASE: u8 = 3;

pub(crate) trait AdaptiveRateObserver: Send + Sync + 'static {
    fn initialized(&self, bitrate: u64);
    fn changed(&self, bitrate: u64, increase: bool);
}

#[derive(Clone, Copy)]
pub(crate) struct RateWindowHealth {
    pub(crate) congested: bool,
    pub(crate) acknowledged_bps: u64,
}

struct GroupRateState {
    reports: Vec<Option<RateWindowHealth>>,
    healthy_rounds: u8,
}

pub(crate) struct AdaptiveRateControl {
    enabled: bool,
    minimum_bitrate: u64,
    maximum_bitrate: u64,
    target_bitrate: AtomicU64,
    observer: Arc<dyn AdaptiveRateObserver>,
    group: Mutex<GroupRateState>,
}

impl AdaptiveRateControl {
    #[cfg(test)]
    pub(crate) fn new<O>(maximum_bitrate: u64, enabled: bool, observer: Arc<O>) -> Self
    where
        O: AdaptiveRateObserver,
    {
        Self::new_group(maximum_bitrate, enabled, observer, 1)
    }

    pub(crate) fn new_group<O>(
        maximum_bitrate: u64,
        enabled: bool,
        observer: Arc<O>,
        participants: usize,
    ) -> Self
    where
        O: AdaptiveRateObserver,
    {
        assert!(participants > 0, "rate-control group must not be empty");
        let minimum_bitrate = (maximum_bitrate / 4).max(500_000).min(maximum_bitrate);
        observer.initialized(maximum_bitrate);
        Self {
            enabled,
            minimum_bitrate,
            maximum_bitrate,
            target_bitrate: AtomicU64::new(maximum_bitrate),
            observer,
            group: Mutex::new(GroupRateState {
                reports: vec![None; participants],
                healthy_rounds: 0,
            }),
        }
    }

    pub(crate) fn target_bitrate(&self) -> u64 {
        self.target_bitrate.load(Ordering::Relaxed)
    }

    pub(crate) fn decrease(&self, _acknowledged_bps: u64) {
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

    pub(crate) fn increase(&self) {
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

    pub(crate) fn report_window(&self, target: usize, health: RateWindowHealth) {
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
            group.reports.fill(None);
            if congested {
                group.healthy_rounds = 0;
                Some(false)
            } else {
                group.healthy_rounds = group.healthy_rounds.saturating_add(1);
                if group.healthy_rounds >= HEALTHY_RATE_WINDOWS_BEFORE_INCREASE {
                    group.healthy_rounds = 0;
                    Some(true)
                } else {
                    None
                }
            }
        };
        match action {
            Some(true) => self.increase(),
            Some(false) => self.decrease(0),
            None => {}
        }
    }

    fn set_target(&self, next: u64, increase: bool) {
        let current = self.target_bitrate();
        if next == current {
            return;
        }
        self.target_bitrate.store(next, Ordering::Relaxed);
        self.observer.changed(next, increase);
        log::debug!(
            "adaptive bitrate changed from {:.2} to {:.2} Mbit/s",
            current as f64 / 1_000_000.0,
            next as f64 / 1_000_000.0
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum LocalOutputControl {
    Volume(f32),
    ToggleMute,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ReceiverVolumeCommand {
    SetLevel(f32),
    ToggleMute,
}

impl From<LocalOutputControl> for ReceiverVolumeCommand {
    fn from(value: LocalOutputControl) -> Self {
        match value {
            LocalOutputControl::Volume(level) => Self::SetLevel(level),
            LocalOutputControl::ToggleMute => Self::ToggleMute,
        }
    }
}

pub(crate) trait ReceiverControlSink {
    fn request_volume(&self, command: ReceiverVolumeCommand);
}

impl<T> ReceiverControlSink for Arc<T>
where
    T: ReceiverControlSink + ?Sized,
{
    fn request_volume(&self, command: ReceiverVolumeCommand) {
        self.as_ref().request_volume(command);
    }
}

pub(crate) fn fan_out_receiver_control<S>(outputs: &[S], command: ReceiverVolumeCommand)
where
    S: ReceiverControlSink,
{
    for output in outputs {
        output.request_volume(command);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OutputSnapshot {
    pub(crate) device_id: u32,
    pub(crate) volume: Option<f32>,
    pub(crate) muted: Option<bool>,
}

pub(crate) struct OutputTracker {
    original: OutputSnapshot,
    observed_volume: Option<f32>,
}

impl OutputTracker {
    const VOLUME_EPSILON: f32 = 0.001;

    pub(crate) fn new(snapshot: OutputSnapshot) -> Result<Self> {
        if snapshot.muted.is_none() {
            bail!("the default output device does not expose a controllable mute state");
        }
        Ok(Self {
            original: snapshot,
            observed_volume: snapshot.volume,
        })
    }

    pub(crate) const fn original(&self) -> OutputSnapshot {
        self.original
    }

    pub(crate) fn observe(&mut self, snapshot: OutputSnapshot) -> (Vec<LocalOutputControl>, bool) {
        let mut controls = Vec::with_capacity(2);
        let mut volume_changed = false;
        if let (Some(previous), Some(volume)) = (self.observed_volume, snapshot.volume)
            && (volume - previous).abs() >= Self::VOLUME_EPSILON
        {
            self.observed_volume = Some(volume);
            volume_changed = true;
            controls.push(LocalOutputControl::Volume(volume.clamp(0.0, 1.0)));
        }
        if snapshot.muted == Some(false) && !volume_changed {
            controls.push(LocalOutputControl::ToggleMute);
        }
        (controls, snapshot.muted == Some(false))
    }
}

pub(crate) trait LocalOutputBackend: Send + 'static {
    fn snapshot(&mut self) -> Result<OutputSnapshot>;
    fn set_volume(&mut self, device_id: u32, volume: f32) -> Result<()>;
    fn set_muted(&mut self, device_id: u32, muted: bool) -> Result<()>;
}

pub(crate) struct LocalOutputRedirect<B: LocalOutputBackend> {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<()>>>,
    _backend: std::marker::PhantomData<B>,
}

impl<B: LocalOutputBackend> LocalOutputRedirect<B> {
    pub(crate) fn start<F>(mut backend: B, poll_interval: Duration, mut control: F) -> Result<Self>
    where
        F: FnMut(LocalOutputControl) + Send + 'static,
    {
        let snapshot = backend.snapshot()?;
        let mut tracker = OutputTracker::new(snapshot)?;
        backend.set_muted(tracker.original().device_id, true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("desktop-local-audio-redirect".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::SeqCst) {
                    thread::sleep(poll_interval);
                    let snapshot = match backend.snapshot() {
                        Ok(snapshot) => snapshot,
                        Err(error) => {
                            log::warn!("could not inspect the local audio output: {error:#}");
                            continue;
                        }
                    };
                    if snapshot.device_id != tracker.original().device_id {
                        restore_output(&mut backend, tracker.original())?;
                        tracker = match OutputTracker::new(snapshot) {
                            Ok(tracker) => tracker,
                            Err(error) => {
                                log::warn!(
                                    "new local audio output cannot be redirected: {error:#}"
                                );
                                return Ok(());
                            }
                        };
                        backend.set_muted(tracker.original().device_id, true)?;
                        continue;
                    }
                    let (events, needs_suppress) = tracker.observe(snapshot);
                    for event in events {
                        control(event);
                    }
                    if needs_suppress
                        && let Err(error) = backend.set_muted(snapshot.device_id, true)
                    {
                        log::warn!("could not keep the local audio output muted: {error:#}");
                    }
                }
                restore_output(&mut backend, tracker.original())
            })
            .context("could not start local audio redirect worker")?;
        Ok(Self {
            stop,
            thread: Some(thread),
            _backend: std::marker::PhantomData,
        })
    }

    pub(crate) fn stop(mut self) -> Result<()> {
        self.finish()
    }

    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_| anyhow!("local audio redirect worker panicked"))?
    }
}

impl<B: LocalOutputBackend> Drop for LocalOutputRedirect<B> {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            log::warn!("could not restore the local audio output: {error:#}");
        }
    }
}

fn restore_output<B: LocalOutputBackend>(backend: &mut B, snapshot: OutputSnapshot) -> Result<()> {
    let mut first = None;
    if let Some(volume) = snapshot.volume
        && let Err(error) = backend.set_volume(snapshot.device_id, volume)
    {
        first = Some(error);
    }
    if let Some(muted) = snapshot.muted
        && let Err(error) = backend.set_muted(snapshot.device_id, muted)
        && first.is_none()
    {
        first = Some(error);
    }
    first.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    use anyhow::Result;

    use super::*;

    #[test]
    fn media_clock_uses_one_epoch_across_timebases() {
        let clock = MediaClock::default();
        assert_eq!(clock.ticks(MediaTimestamp::new(10, 1), 90_000), Some(0));
        assert_eq!(
            clock.ticks(MediaTimestamp::new(21, 2), 48_000),
            Some(24_000)
        );
        assert_eq!(
            clock.ticks(MediaTimestamp::new(11, 1), 90_000),
            Some(90_000)
        );
        assert_eq!(clock.ticks(MediaTimestamp::invalid(), 90_000), None);
    }

    #[test]
    fn output_tracker_forwards_volume_changes_and_reasserts_local_mute() {
        let original = OutputSnapshot {
            device_id: 7,
            volume: Some(0.4),
            muted: Some(false),
        };
        let mut tracker = OutputTracker::new(original).unwrap();
        assert_eq!(tracker.original(), original);
        let (controls, needs_suppress) = tracker.observe(OutputSnapshot {
            device_id: 7,
            volume: Some(0.55),
            muted: Some(false),
        });
        assert_eq!(controls, vec![LocalOutputControl::Volume(0.55)]);
        assert!(needs_suppress);

        let (controls, needs_suppress) = tracker.observe(OutputSnapshot {
            device_id: 7,
            volume: Some(0.55),
            muted: Some(false),
        });
        assert_eq!(controls, vec![LocalOutputControl::ToggleMute]);
        assert!(needs_suppress);
    }

    #[test]
    fn output_tracker_rejects_devices_without_mute_control() {
        let error = OutputTracker::new(OutputSnapshot {
            device_id: 7,
            volume: Some(0.4),
            muted: None,
        })
        .err()
        .unwrap();
        assert!(error.to_string().contains("controllable mute"));
    }

    #[derive(Clone)]
    struct FakeReceiverControl {
        commands: Arc<Mutex<Vec<ReceiverVolumeCommand>>>,
    }

    impl ReceiverControlSink for FakeReceiverControl {
        fn request_volume(&self, command: ReceiverVolumeCommand) {
            self.commands.lock().unwrap().push(command);
        }
    }

    #[test]
    fn receiver_controls_map_and_fan_out_to_every_target() {
        let first = Arc::new(Mutex::new(Vec::new()));
        let second = Arc::new(Mutex::new(Vec::new()));
        let outputs = [
            FakeReceiverControl {
                commands: Arc::clone(&first),
            },
            FakeReceiverControl {
                commands: Arc::clone(&second),
            },
        ];

        fan_out_receiver_control(&outputs, LocalOutputControl::Volume(0.65).into());
        fan_out_receiver_control(&outputs, LocalOutputControl::ToggleMute.into());

        let expected = vec![
            ReceiverVolumeCommand::SetLevel(0.65),
            ReceiverVolumeCommand::ToggleMute,
        ];
        assert_eq!(*first.lock().unwrap(), expected);
        assert_eq!(*second.lock().unwrap(), expected);
    }

    struct FakeOutputState {
        snapshot: OutputSnapshot,
        volume_updates: Vec<f32>,
        mute_updates: Vec<bool>,
    }

    struct FakeOutputBackend {
        state: Arc<Mutex<FakeOutputState>>,
    }

    impl LocalOutputBackend for FakeOutputBackend {
        fn snapshot(&mut self) -> Result<OutputSnapshot> {
            Ok(self.state.lock().unwrap().snapshot)
        }

        fn set_volume(&mut self, _device_id: u32, volume: f32) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            state.snapshot.volume = Some(volume);
            state.volume_updates.push(volume);
            Ok(())
        }

        fn set_muted(&mut self, _device_id: u32, muted: bool) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            state.snapshot.muted = Some(muted);
            state.mute_updates.push(muted);
            Ok(())
        }
    }

    #[test]
    fn output_redirect_uses_a_backend_and_restores_the_original_state() {
        let original = OutputSnapshot {
            device_id: 7,
            volume: Some(0.4),
            muted: Some(false),
        };
        let state = Arc::new(Mutex::new(FakeOutputState {
            snapshot: original,
            volume_updates: Vec::new(),
            mute_updates: Vec::new(),
        }));
        let (control_tx, control_rx) = mpsc::channel();
        let redirect = LocalOutputRedirect::start(
            FakeOutputBackend {
                state: Arc::clone(&state),
            },
            Duration::from_millis(1),
            move |control| control_tx.send(control).unwrap(),
        )
        .unwrap();
        {
            let mut state = state.lock().unwrap();
            assert_eq!(state.snapshot.muted, Some(true));
            state.snapshot.volume = Some(0.6);
            state.snapshot.muted = Some(false);
        }
        assert_eq!(
            control_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            LocalOutputControl::Volume(0.6)
        );
        redirect.stop().unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.snapshot, original);
        assert_eq!(state.volume_updates.last(), Some(&0.4));
        assert_eq!(state.mute_updates.last(), Some(&false));
    }

    #[test]
    fn synthetic_cycle_visits_each_phase_and_repeats() {
        let fps = 30;
        assert_eq!(phase_for_frame(0, fps).0, SyntheticPhase::Static);
        assert_eq!(phase_for_frame(75, fps).0, SyntheticPhase::PartialMotion);
        assert_eq!(phase_for_frame(150, fps).0, SyntheticPhase::FullMotion);
        assert_eq!(phase_for_frame(225, fps).0, SyntheticPhase::SceneCuts);
        assert_eq!(phase_for_frame(300, fps).0, SyntheticPhase::Static);
    }

    #[test]
    fn selects_h264_levels_without_platform_encoder_types() {
        assert_eq!(
            H264Level::for_stream(1280, 720, 30, 6_000_000)
                .unwrap()
                .name,
            "3.1"
        );
        assert_eq!(
            H264Level::for_stream(1280, 720, 60, 6_000_000)
                .unwrap()
                .codec_parameter,
            "avc1.420020"
        );
        assert_eq!(
            H264Level::for_stream(1920, 1080, 30, 6_000_000)
                .unwrap()
                .name,
            "4.0"
        );
        assert!(H264Level::for_stream(7680, 4320, 120, 250_000_000).is_err());
    }

    #[derive(Clone)]
    struct FakeSink {
        frames: Rc<RefCell<Vec<EncodedVideoFrame>>>,
        fail: bool,
    }

    impl EncodedVideoSink for FakeSink {
        fn submit_video(&self, frame: EncodedVideoFrame) -> Result<()> {
            if self.fail {
                bail!("fake receiver failed");
            }
            self.frames.borrow_mut().push(frame);
            Ok(())
        }
    }

    #[test]
    fn encoded_frames_fan_out_in_order_and_propagate_failure() {
        let first = Rc::new(RefCell::new(Vec::new()));
        let second = Rc::new(RefCell::new(Vec::new()));
        let frame = EncodedVideoFrame {
            rtp_timestamp: 90_000,
            keyframe: true,
            data: Arc::new(vec![0, 0, 0, 1, 0x65]),
            timings: VideoFrameTimings {
                pipeline_started_at: Instant::now(),
                capture_age_micros: Some(1),
                queue_wait_micros: 2,
                encode_micros: 3,
                prepare_micros: 4,
                sender_lock_wait_micros: 0,
            },
            synthetic_phase: Some(SyntheticPhase::SceneCuts),
        };
        fan_out_video(
            &[
                FakeSink {
                    frames: Rc::clone(&first),
                    fail: false,
                },
                FakeSink {
                    frames: Rc::clone(&second),
                    fail: false,
                },
            ],
            frame.clone(),
        )
        .unwrap();
        assert_eq!(first.borrow()[0].rtp_timestamp, 90_000);
        assert_eq!(second.borrow()[0].data.as_slice(), frame.data.as_slice());

        let error = fan_out_video(
            &[FakeSink {
                frames: second,
                fail: true,
            }],
            frame,
        )
        .unwrap_err();
        assert!(error.to_string().contains("fake receiver failed"));
    }

    #[derive(Clone)]
    struct FakeAudioSink {
        frames: Rc<RefCell<Vec<EncodedAudioFrame>>>,
    }

    impl EncodedAudioSink for FakeAudioSink {
        fn submit_audio(&self, frame: EncodedAudioFrame) -> Result<()> {
            self.frames.borrow_mut().push(frame);
            Ok(())
        }
    }

    #[test]
    fn encoded_audio_uses_the_same_multi_receiver_boundary() {
        let first = Rc::new(RefCell::new(Vec::new()));
        let second = Rc::new(RefCell::new(Vec::new()));
        let frame = EncodedAudioFrame {
            timestamp: 48_000,
            data: vec![0x11, 0x90],
        };
        fan_out_audio(
            &[
                FakeAudioSink {
                    frames: Rc::clone(&first),
                },
                FakeAudioSink {
                    frames: Rc::clone(&second),
                },
            ],
            frame,
        )
        .unwrap();
        assert_eq!(first.borrow()[0].timestamp, 48_000);
        assert_eq!(second.borrow()[0].data, vec![0x11, 0x90]);
    }

    #[derive(Default)]
    struct FakeEncoderControl {
        bitrates: RefCell<Vec<u32>>,
        forced_keyframes: Cell<u32>,
    }

    impl VideoEncoderControl for FakeEncoderControl {
        fn set_bitrate(&self, bitrate: u32) -> Result<()> {
            self.bitrates.borrow_mut().push(bitrate);
            Ok(())
        }

        fn force_keyframe(&self) -> Result<()> {
            self.forced_keyframes
                .set(self.forced_keyframes.get().saturating_add(1));
            Ok(())
        }
    }

    #[test]
    fn encoder_control_boundary_supports_rate_and_keyframe_requests() {
        let encoder = FakeEncoderControl::default();
        encoder.set_bitrate(4_800_000).unwrap();
        encoder.force_keyframe().unwrap();
        assert_eq!(*encoder.bitrates.borrow(), vec![4_800_000]);
        assert_eq!(encoder.forced_keyframes.get(), 1);
    }

    struct FakeBackend {
        has_reference: Arc<AtomicBool>,
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        processed: mpsc::Sender<u64>,
        fail: bool,
    }

    impl LatestFrameBackend for FakeBackend {
        type Frame = u64;

        fn has_reference_frame(&self) -> bool {
            self.has_reference.load(Ordering::SeqCst)
        }

        fn process_frame(&mut self, frame: Self::Frame, _queue_wait_micros: u64) -> Result<()> {
            if self.fail {
                bail!("fake encoder failure");
            }
            self.processed.send(frame).unwrap();
            if frame == 1 {
                self.has_reference.store(true, Ordering::SeqCst);
                self.started.send(()).unwrap();
                self.release.recv().unwrap();
            }
            Ok(())
        }
    }

    #[test]
    fn latest_frame_worker_replaces_pending_frames_and_expires_stale_deltas() {
        let metrics = Arc::new(LatestFrameMetrics::default());
        let observer: Arc<dyn LatestFrameObserver> = metrics.clone();
        let failure = Arc::new(Mutex::new(None));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (processed_tx, processed_rx) = mpsc::channel();
        let backend = FakeBackend {
            has_reference: Arc::new(AtomicBool::new(false)),
            started: started_tx,
            release: release_rx,
            processed: processed_tx,
            fail: false,
        };
        let (submitter, mut worker) = LatestFrameWorker::start(
            backend,
            Some(Duration::from_millis(1)),
            Arc::clone(&failure),
            observer,
            "fake-latest-frame",
        )
        .unwrap();
        submitter.submit(1).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        submitter.submit(2).unwrap();
        submitter.submit(3).unwrap();
        thread::sleep(Duration::from_millis(5));
        release_tx.send(()).unwrap();
        assert_eq!(
            processed_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            1
        );
        assert!(
            processed_rx
                .recv_timeout(Duration::from_millis(20))
                .is_err()
        );
        worker.stop().unwrap();
        assert_eq!(metrics.snapshot(), (3, 1, 1));
        assert!(failure.lock().unwrap().is_none());
    }

    #[test]
    fn latest_frame_worker_reports_backend_failures_and_stops_accepting_frames() {
        let metrics = Arc::new(LatestFrameMetrics::default());
        let observer: Arc<dyn LatestFrameObserver> = metrics.clone();
        let failure = Arc::new(Mutex::new(None));
        let (started_tx, _started_rx) = mpsc::channel();
        let (_release_tx, release_rx) = mpsc::channel();
        let (processed_tx, _processed_rx) = mpsc::channel();
        let backend = FakeBackend {
            has_reference: Arc::new(AtomicBool::new(false)),
            started: started_tx,
            release: release_rx,
            processed: processed_tx,
            fail: true,
        };
        let (submitter, mut worker) = LatestFrameWorker::start(
            backend,
            None,
            Arc::clone(&failure),
            observer,
            "fake-failing-frame",
        )
        .unwrap();
        submitter.submit(1).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while failure.lock().unwrap().is_none() && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(
            failure
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|message| message.contains("fake encoder failure"))
        );
        worker.stop().unwrap();
        assert_eq!(metrics.snapshot(), (1, 0, 0));
        assert!(submitter.submit(2).is_err());
    }

    #[derive(Default)]
    struct FakeRateObserver {
        initialized: AtomicU64,
        current: AtomicU64,
        increases: AtomicU64,
        decreases: AtomicU64,
    }

    impl AdaptiveRateObserver for FakeRateObserver {
        fn initialized(&self, bitrate: u64) {
            self.initialized.store(bitrate, Ordering::Relaxed);
            self.current.store(bitrate, Ordering::Relaxed);
        }

        fn changed(&self, bitrate: u64, increase: bool) {
            self.current.store(bitrate, Ordering::Relaxed);
            if increase {
                self.increases.fetch_add(1, Ordering::Relaxed);
            } else {
                self.decreases.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn adaptive_rate_control_decreases_fast_and_increases_slowly() {
        let observer = Arc::new(FakeRateObserver::default());
        let rate = AdaptiveRateControl::new(6_000_000, true, Arc::clone(&observer));
        assert_eq!(observer.initialized.load(Ordering::Relaxed), 6_000_000);
        rate.report_window(
            0,
            RateWindowHealth {
                congested: true,
                acknowledged_bps: 3_000_000,
            },
        );
        assert_eq!(rate.target_bitrate(), 4_800_000);
        for _ in 0..3 {
            rate.report_window(
                0,
                RateWindowHealth {
                    congested: false,
                    acknowledged_bps: 4_800_000,
                },
            );
        }
        assert_eq!(rate.target_bitrate(), 5_040_000);
        assert_eq!(observer.decreases.load(Ordering::Relaxed), 1);
        assert_eq!(observer.increases.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn grouped_rate_control_waits_for_every_receiver_and_uses_congestion() {
        let observer = Arc::new(FakeRateObserver::default());
        let rate = AdaptiveRateControl::new_group(6_000_000, true, observer, 2);
        let healthy = RateWindowHealth {
            congested: false,
            acknowledged_bps: 6_000_000,
        };
        rate.report_window(0, healthy);
        assert_eq!(rate.target_bitrate(), 6_000_000);
        rate.report_window(
            1,
            RateWindowHealth {
                congested: true,
                acknowledged_bps: 3_000_000,
            },
        );
        assert_eq!(rate.target_bitrate(), 4_800_000);
    }
}
