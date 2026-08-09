use std::{
    io,
    sync::mpsc::{self, Receiver},
};

#[cfg(target_os = "macos")]
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum SystemVolumeEvent {
    Volume(f32),
    Muted(bool),
}

pub struct SystemVolumeMonitor {
    events: Receiver<SystemVolumeEvent>,
    #[cfg(target_os = "macos")]
    stop: Arc<AtomicBool>,
    #[cfg(target_os = "macos")]
    worker: Option<JoinHandle<()>>,
}

impl SystemVolumeMonitor {
    pub fn start() -> io::Result<Self> {
        let (sender, events) = mpsc::channel();
        #[cfg(target_os = "macos")]
        {
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = Arc::clone(&stop);
            let worker = thread::Builder::new()
                .name("macos-system-volume".into())
                .spawn(move || {
                    let mut previous = None;
                    while !worker_stop.load(Ordering::SeqCst) {
                        thread::park_timeout(Duration::from_millis(50));
                        let current = match output_snapshot() {
                            Ok(current) => current,
                            Err(status) => {
                                log::debug!(
                                    "could not inspect macOS system volume for TUI controls ({status})"
                                );
                                continue;
                            }
                        };
                        let Some(last) = previous.replace(current) else {
                            continue;
                        };
                        for event in changed_events(last, current) {
                            if sender.send(event).is_err() {
                                return;
                            }
                        }
                    }
                })?;
            Ok(Self {
                events,
                stop,
                worker: Some(worker),
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            drop(sender);
            Ok(Self { events })
        }
    }

    pub fn drain_events(&self, limit: usize) -> Vec<SystemVolumeEvent> {
        self.events.try_iter().take(limit).collect()
    }
}

#[cfg(target_os = "macos")]
impl Drop for SystemVolumeMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            if worker.join().is_err() {
                log::warn!("macOS system-volume monitor panicked during shutdown");
            }
        }
    }
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug)]
struct OutputSnapshot {
    device_id: u32,
    volume: Option<f32>,
    muted: Option<bool>,
}

#[cfg(any(target_os = "macos", test))]
fn changed_events(previous: OutputSnapshot, current: OutputSnapshot) -> Vec<SystemVolumeEvent> {
    if previous.device_id != current.device_id {
        return Vec::new();
    }
    let mut events = Vec::with_capacity(2);
    if let (Some(previous), Some(current)) = (previous.volume, current.volume)
        && (current - previous).abs() >= 0.001
    {
        events.push(SystemVolumeEvent::Volume(current.clamp(0.0, 1.0)));
    }
    if let (Some(previous), Some(current)) = (previous.muted, current.muted)
        && current != previous
    {
        events.push(SystemVolumeEvent::Muted(current));
    }
    events
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Default)]
struct NativeAudioOutputState {
    device_id: u32,
    volume: f32,
    muted: u32,
    has_volume: u32,
    has_mute: u32,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn cast_audio_output_snapshot(state: *mut NativeAudioOutputState) -> i32;
}

#[cfg(target_os = "macos")]
fn output_snapshot() -> Result<OutputSnapshot, i32> {
    let mut state = NativeAudioOutputState::default();
    let status = unsafe { cast_audio_output_snapshot(&mut state) };
    if status != 0 {
        return Err(status);
    }
    Ok(OutputSnapshot {
        device_id: state.device_id,
        volume: (state.has_volume != 0).then_some(state.volume),
        muted: (state.has_mute != 0).then_some(state.muted != 0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_only_supported_changed_volume_properties() {
        let events = changed_events(
            OutputSnapshot {
                device_id: 1,
                volume: Some(0.4),
                muted: Some(false),
            },
            OutputSnapshot {
                device_id: 1,
                volume: Some(0.55),
                muted: Some(true),
            },
        );
        assert_eq!(
            events,
            vec![
                SystemVolumeEvent::Volume(0.55),
                SystemVolumeEvent::Muted(true)
            ]
        );

        assert!(
            changed_events(
                OutputSnapshot {
                    device_id: 1,
                    volume: None,
                    muted: None,
                },
                OutputSnapshot {
                    device_id: 1,
                    volume: None,
                    muted: None,
                }
            )
            .is_empty()
        );

        assert!(
            changed_events(
                OutputSnapshot {
                    device_id: 1,
                    volume: Some(0.4),
                    muted: Some(false),
                },
                OutputSnapshot {
                    device_id: 2,
                    volume: Some(0.8),
                    muted: Some(true),
                }
            )
            .is_empty()
        );
    }
}
