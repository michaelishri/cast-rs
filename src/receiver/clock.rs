use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The master playback clock. With an audio output device, played samples
/// drive the time; a wall clock stands in when no device is available. All
/// times are tracked in milliseconds.
pub struct Clock {
    inner: Arc<ClockInner>,
}

struct ClockInner {
    /// > 0 while playback timing follows consumed audio frames; 0 means the
    /// > wall clock drives the media time. Stored as f64 bits.
    sample_rate_bits: AtomicU64,
    base_millis: AtomicU64,
    base_samples: AtomicU64,
    samples: AtomicU64,
    wall_anchor_millis: AtomicU64,
    playing: AtomicBool,
}

impl Clone for Clock {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Clock {
    pub fn sample_clock(sample_rate: u32) -> Self {
        Self {
            inner: Arc::new(ClockInner {
                sample_rate_bits: AtomicU64::new(f64::from(sample_rate.max(1)).to_bits()),
                base_millis: AtomicU64::new(0),
                base_samples: AtomicU64::new(0),
                samples: AtomicU64::new(0),
                wall_anchor_millis: AtomicU64::new(0),
                playing: AtomicBool::new(false),
            }),
        }
    }

    pub fn wall_clock() -> Self {
        Self {
            inner: Arc::new(ClockInner {
                sample_rate_bits: AtomicU64::new(0),
                base_millis: AtomicU64::new(0),
                base_samples: AtomicU64::new(0),
                samples: AtomicU64::new(0),
                wall_anchor_millis: AtomicU64::new(0),
                playing: AtomicBool::new(false),
            }),
        }
    }

    pub fn sample_mode(&self) -> bool {
        self.inner.sample_rate_bits.load(Ordering::SeqCst) != 0
    }

    /// Switches between sample-driven and wall-driven timing; used when the
    /// media turns out to have no audio track to pace playback with.
    pub fn set_sample_mode(&self, sample_mode: bool) {
        let inner = &self.inner;
        let currently_sample = inner.sample_rate_bits.load(Ordering::SeqCst) != 0;
        if currently_sample == sample_mode {
            return;
        }
        self.rebase(self.media_time_millis());
        if !sample_mode {
            inner.sample_rate_bits.store(0, Ordering::SeqCst);
        }
    }

    /// Records `frames` consumed frames of playback (sample mode only).
    pub fn advance_frames(&self, frames: u64) {
        if self.sample_mode() {
            self.inner.samples.fetch_add(frames, Ordering::Relaxed);
        }
    }

    /// Re-anchors the clock at an explicit media time.
    fn rebase(&self, base_millis: u64) {
        let inner = &self.inner;
        inner.base_millis.store(base_millis, Ordering::SeqCst);
        inner
            .base_samples
            .store(inner.samples.load(Ordering::Relaxed), Ordering::SeqCst);
        inner
            .wall_anchor_millis
            .store(unix_millis(), Ordering::SeqCst);
    }

    /// Current media time in milliseconds.
    pub fn media_time_millis(&self) -> u64 {
        let inner = &self.inner;
        if !inner.playing.load(Ordering::SeqCst) {
            return inner.base_millis.load(Ordering::SeqCst);
        }
        let sample_rate_bits = inner.sample_rate_bits.load(Ordering::SeqCst);
        if sample_rate_bits != 0 {
            let rate = f64::from_bits(sample_rate_bits);
            let base = inner.base_millis.load(Ordering::SeqCst);
            let frames = inner
                .samples
                .load(Ordering::Relaxed)
                .saturating_sub(inner.base_samples.load(Ordering::SeqCst));
            base + (frames as f64 / rate * 1000.0) as u64
        } else {
            let now = unix_millis();
            let elapsed = now.saturating_sub(inner.wall_anchor_millis.load(Ordering::SeqCst));
            inner.base_millis.load(Ordering::SeqCst) + elapsed
        }
    }

    pub fn media_time_secs(&self) -> f64 {
        self.media_time_millis() as f64 / 1000.0
    }

    pub fn set_playing(&self, playing: bool) {
        // Freeze the current media time as the new base before toggling.
        self.rebase(self.media_time_millis());
        self.inner.playing.store(playing, Ordering::SeqCst);
    }

    pub fn seek_to(&self, secs: f64) {
        self.rebase((secs.max(0.0) * 1000.0) as u64);
    }

    #[allow(dead_code)] // exercised by unit tests
    pub fn is_playing(&self) -> bool {
        self.inner.playing.load(Ordering::SeqCst)
    }
}

pub fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// A bounded ring of interleaved f32 samples shared between the decode
/// thread (producer) and the audio callback (consumer). Locking is coarse but
/// the critical sections are pure memcpy and both sides degrade gracefully:
/// the callback outputs silence on contention and the producer retries.
pub struct SampleRing {
    inner: Mutex<RingState>,
}

struct RingState {
    buffer: Vec<f32>,
    read: usize,
    write: usize,
}

impl SampleRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(RingState {
                buffer: vec![0.0; capacity],
                read: 0,
                write: 0,
            }),
        }
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .map(|state| state.write - state.read)
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Pushes as many samples as fit; returns the count accepted.
    pub fn push(&self, data: &[f32]) -> usize {
        let mut state = match self.inner.lock() {
            Ok(state) => state,
            Err(_) => return 0,
        };
        let used = state.write - state.read;
        let free = state.buffer.len() - used;
        let count = data.len().min(free);
        if count == 0 {
            return 0;
        }
        let write_index = state.write % state.buffer.len();
        let first = (state.buffer.len() - write_index).min(count);
        state.buffer[write_index..write_index + first].copy_from_slice(&data[..first]);
        if count > first {
            state.buffer[..count - first].copy_from_slice(&data[first..count]);
        }
        state.write += count;
        count
    }

    pub fn pop(&self, out: &mut [f32]) -> usize {
        let mut state = match self.inner.lock() {
            Ok(state) => state,
            Err(_) => return 0,
        };
        let count = out.len().min(state.write - state.read);
        if count == 0 {
            return 0;
        }
        let read_index = state.read % state.buffer.len();
        let first = (state.buffer.len() - read_index).min(count);
        out[..first].copy_from_slice(&state.buffer[read_index..read_index + first]);
        if count > first {
            out[first..count].copy_from_slice(&state.buffer[..count - first]);
        }
        state.read += count;
        // Keep both counters bounded; they always move forward together.
        if state.read >= state.buffer.len() {
            state.read -= state.buffer.len();
            state.write -= state.buffer.len();
        }
        count
    }

    #[allow(dead_code)] // exercised by unit tests
    pub fn clear(&self) {
        if let Ok(mut state) = self.inner.lock() {
            state.read = state.write;
        }
    }
}

/// Loudness controls applied by the audio callback.
#[derive(Default)]
pub struct VolumeState {
    level_bits: AtomicU64,
    muted: AtomicBool,
}

impl VolumeState {
    pub fn new(level: f64, muted: bool) -> Self {
        Self {
            level_bits: AtomicU64::new(level.clamp(0.0, 1.0).to_bits()),
            muted: AtomicBool::new(muted),
        }
    }

    pub fn set(&self, level: f64, muted: bool) {
        self.level_bits
            .store(level.clamp(0.0, 1.0).to_bits(), Ordering::SeqCst);
        self.muted.store(muted, Ordering::SeqCst);
    }

    #[allow(dead_code)] // exercised by unit tests
    pub fn level(&self) -> f64 {
        f64::from_bits(self.level_bits.load(Ordering::SeqCst))
    }

    #[allow(dead_code)] // exercised by unit tests
    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn sample_clock_tracks_consumed_frames() {
        let clock = Clock::sample_clock(48_000);
        clock.set_playing(true);
        assert_eq!(clock.media_time_millis(), 0);
        clock.advance_frames(48_000);
        assert_eq!(clock.media_time_millis(), 1000);
        clock.advance_frames(24_000);
        assert_eq!(clock.media_time_millis(), 1500);

        clock.set_playing(false);
        let frozen = clock.media_time_millis();
        clock.advance_frames(48_000);
        assert_eq!(clock.media_time_millis(), frozen);
        clock.set_playing(true);
        assert_eq!(clock.media_time_millis(), 1500);
    }

    #[test]
    fn wall_clock_advances_while_playing_and_freezes_when_paused() {
        let clock = Clock::wall_clock();
        clock.seek_to(10.0);
        assert_eq!(clock.media_time_millis(), 10_000);
        clock.set_playing(true);
        std::thread::sleep(Duration::from_millis(60));
        let elapsed = clock.media_time_millis();
        assert!(elapsed >= 10_050, "wall clock did not advance: {elapsed}");
        clock.set_playing(false);
        let frozen = clock.media_time_millis();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(clock.media_time_millis(), frozen);
    }

    #[test]
    fn seeks_rebase_both_clock_modes() {
        let clock = Clock::sample_clock(48_000);
        clock.set_playing(true);
        clock.advance_frames(48_000);
        clock.seek_to(42.5);
        assert_eq!(clock.media_time_millis(), 42_500);
        clock.advance_frames(48_000);
        assert_eq!(clock.media_time_millis(), 43_500);

        let wall = Clock::wall_clock();
        wall.set_playing(true);
        wall.seek_to(7.25);
        assert_eq!(wall.media_time_millis(), 7_250);
    }

    #[test]
    fn ring_preserves_order_across_the_wraparound() {
        let ring = SampleRing::new(8);
        assert_eq!(ring.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 6);
        let mut out = [0.0; 4];
        assert_eq!(ring.pop(&mut out), 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(ring.push(&[7.0, 8.0, 9.0, 10.0, 11.0]), 5);
        let mut out = [0.0; 6];
        assert_eq!(ring.pop(&mut out), 6);
        assert_eq!(out, [5.0, 6.0, 7.0, 8.0, 9.0, 10.0]);
        assert_eq!(ring.len(), 1);
        let mut out = [0.0; 2];
        assert_eq!(ring.pop(&mut out), 1);
        assert_eq!(out[0], 11.0);
        assert!(ring.is_empty());
    }

    #[test]
    fn ring_rejects_writes_beyond_capacity() {
        let ring = SampleRing::new(4);
        assert_eq!(ring.push(&[1.0, 2.0, 3.0, 4.0]), 4);
        assert_eq!(ring.push(&[5.0]), 0);
        let mut out = [0.0; 2];
        assert_eq!(ring.pop(&mut out), 2);
        assert_eq!(ring.push(&[5.0, 6.0, 7.0]), 2);
        let mut out = [0.0; 4];
        assert_eq!(ring.pop(&mut out), 4);
        assert_eq!(out, [3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn ring_clear_drops_pending_samples() {
        let ring = SampleRing::new(16);
        ring.push(&[1.0, 2.0, 3.0]);
        ring.clear();
        assert!(ring.is_empty());
    }

    #[test]
    fn volume_state_clamps_and_reports() {
        let volume = VolumeState::new(1.5, false);
        assert_eq!(volume.level(), 1.0);
        volume.set(-1.0, true);
        assert_eq!(volume.level(), 0.0);
        assert!(volume.muted());
        volume.set(0.5, false);
        assert!(!volume.muted());
    }
}
