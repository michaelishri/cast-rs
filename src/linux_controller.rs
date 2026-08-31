use std::{
    fs, process,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};

pub(crate) struct ControllerWatch {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ControllerWatch {
    pub(crate) fn start(controller_pid: Option<u32>) -> Result<Option<Self>> {
        let Some(controller_pid) = controller_pid else {
            return Ok(None);
        };
        if controller_pid <= 1 || controller_pid == process::id() {
            bail!("--controller-pid must identify another live user process");
        }
        let expected_start = process_start_time(controller_pid).with_context(|| {
            format!("controller process {controller_pid} is not running or is inaccessible")
        })?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("cast-controller-watch".to_owned())
            .spawn(move || {
                while !worker_stop.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(250));
                    let controller_is_alive = process_start_time(controller_pid)
                        .is_ok_and(|start| start == expected_start);
                    if !controller_is_alive {
                        log::info!(
                            "desktop controller process {controller_pid} exited; stopping cast"
                        );
                        // SAFETY: raise targets this process with a standard signal. The desktop
                        // transports install a SIGINT handler before allocating capture resources.
                        unsafe {
                            libc::raise(libc::SIGINT);
                        }
                        break;
                    }
                }
            })
            .context("could not start the desktop controller watcher")?;
        Ok(Some(Self {
            stop,
            worker: Some(worker),
        }))
    }
}

impl Drop for ControllerWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            log::warn!("desktop controller watcher panicked");
        }
    }
}

fn process_start_time(pid: u32) -> Result<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    parse_process_start_time(&stat)
}

fn parse_process_start_time(stat: &str) -> Result<String> {
    let fields = stat
        .rsplit_once(')')
        .map(|(_, fields)| fields)
        .ok_or_else(|| anyhow!("malformed Linux process stat record"))?;
    fields
        .split_whitespace()
        .nth(19)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("Linux process stat record has no start time"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_start_time_after_a_parenthesized_process_name() {
        let mut fields = vec!["S"; 20];
        fields[19] = "987654";
        let stat = format!("42 (name with ) paren) {}", fields.join(" "));
        assert_eq!(parse_process_start_time(&stat).unwrap(), "987654");
    }

    #[test]
    fn current_process_has_a_stable_identity() {
        let first = process_start_time(process::id()).unwrap();
        let second = process_start_time(process::id()).unwrap();
        assert_eq!(first, second);
    }
}
