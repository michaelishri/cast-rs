use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result, bail};

pub(crate) struct ControllerWatch {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ControllerWatch {
    pub(crate) fn start(controller_pid: Option<u32>) -> Result<Option<Self>> {
        let Some(controller_pid) = controller_pid else {
            return Ok(None);
        };
        let controller_pid = libc::pid_t::try_from(controller_pid)
            .context("--controller-pid exceeds the macOS process ID range")?;
        if controller_pid <= 1 || controller_pid == unsafe { libc::getpid() } {
            bail!("--controller-pid must identify the direct parent process");
        }
        if parent_pid() != controller_pid {
            bail!("--controller-pid must identify the direct parent process");
        }

        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("cast-controller-watch".to_owned())
            .spawn(move || {
                while !worker_stop.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(250));
                    if parent_pid() != controller_pid {
                        log::info!(
                            "desktop controller process {controller_pid} exited; stopping cast"
                        );
                        // SAFETY: raise targets this process with SIGINT. Both desktop transports
                        // install their interrupt handler before allocating capture resources.
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

fn parent_pid() -> libc::pid_t {
    // SAFETY: getppid has no preconditions and does not dereference pointers.
    unsafe { libc::getppid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_current_direct_parent() {
        let watch = ControllerWatch::start(Some(parent_pid() as u32)).unwrap();
        assert!(watch.is_some());
    }

    #[test]
    fn rejects_the_current_process() {
        let error = ControllerWatch::start(Some(std::process::id()))
            .err()
            .expect("the current process is not its own parent");
        assert!(error.to_string().contains("direct parent"));
    }
}
