use std::{
    ffi::{CStr, c_char},
    io::{self, BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use screencapturekit::prelude::*;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn cast_virtual_display_create(
        width: u32,
        height: u32,
        frames_per_second: u32,
        error_buffer: *mut c_char,
        error_buffer_length: usize,
    ) -> u32;
    fn cast_virtual_display_destroy();
    fn cast_virtual_display_is_online(display_id: u32) -> bool;
}

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const SHAREABLE_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(7);

pub struct VirtualDisplaySession {
    display_id: u32,
    child: Child,
    lifetime: Option<ChildStdin>,
    stopped: bool,
}

impl VirtualDisplaySession {
    pub fn start(width: u32, height: u32, fps: u32) -> Result<Self> {
        let executable = std::env::current_exe()
            .context("could not locate the Cast executable for the virtual display helper")?;
        let mut command = Command::new(executable);
        let helper_stderr = if log::log_enabled!(log::Level::Debug) {
            // Surface helper failures alongside the parent's timing at -v/-vv.
            Stdio::inherit()
        } else {
            Stdio::null()
        };
        command
            .arg("__virtual-display-helper")
            .arg("--width")
            .arg(width.to_string())
            .arg("--height")
            .arg(height.to_string())
            .arg("--fps")
            .arg(fps.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(helper_stderr);

        // Keep Ctrl-C in the interactive parent from terminating the helper
        // before the parent has stopped capture and closed its lifetime pipe.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command
            .spawn()
            .context("could not start the temporary virtual display helper")?;
        let Some(lifetime) = child.stdin.take() else {
            terminate_child(&mut child);
            bail!("virtual display helper stdin was not available");
        };
        let Some(stdout) = child.stdout.take() else {
            drop(lifetime);
            terminate_child(&mut child);
            bail!("virtual display helper stdout was not available");
        };

        let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
        let startup_reader = thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout)
                .read_line(&mut line)
                .map(|read| (read, line));
            let _ = startup_sender.send(result);
        });

        let startup = match startup_receiver.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok((0, _))) => Err(anyhow!(
                "the virtual display helper exited before reporting its display ID"
            )),
            Ok(Ok((_, line))) => parse_startup_message(&line),
            Ok(Err(error)) => Err(error).context("could not read from the virtual display helper"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(anyhow!("timed out starting the temporary virtual display"))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(anyhow!(
                "the virtual display helper startup channel disconnected"
            )),
        };

        let display_id = match startup {
            Ok(display_id) => {
                if startup_reader.join().is_err() {
                    drop(lifetime);
                    terminate_child(&mut child);
                    bail!("virtual display helper reader thread panicked");
                }
                display_id
            }
            Err(error) => {
                drop(lifetime);
                terminate_child(&mut child);
                startup_reader
                    .join()
                    .map_err(|_| anyhow!("virtual display helper reader thread panicked"))?;
                return Err(error);
            }
        };

        let mut session = Self {
            display_id,
            child,
            lifetime: Some(lifetime),
            stopped: false,
        };
        if let Err(error) = session.wait_until_shareable() {
            if let Err(cleanup_error) = session.stop_inner(false) {
                log::warn!(
                    "could not clean up virtual display {display_id} after startup failed: {cleanup_error:#}"
                );
            }
            return Err(error);
        }

        println!("Created temporary extended display {display_id} at {width}x{height}, {fps} fps.");
        Ok(session)
    }

    pub const fn display_id(&self) -> u32 {
        self.display_id
    }

    pub fn ensure_alive(&mut self) -> Result<()> {
        if self.stopped {
            bail!(
                "temporary virtual display {} is no longer active",
                self.display_id
            );
        }
        if let Some(status) = self
            .child
            .try_wait()
            .context("could not query the virtual display helper")?
        {
            self.lifetime.take();
            bail!(
                "temporary virtual display {} disappeared because its helper exited with {status}",
                self.display_id
            );
        }
        if !native_is_online(self.display_id) {
            bail!(
                "temporary virtual display {} is no longer online",
                self.display_id
            );
        }
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        self.stop_inner(true)
    }

    fn wait_until_shareable(&mut self) -> Result<()> {
        let started = Instant::now();
        loop {
            self.ensure_alive()?;
            match SCShareableContent::get() {
                Ok(content)
                    if content
                        .displays()
                        .iter()
                        .any(|display| display.display_id() == self.display_id) =>
                {
                    return Ok(());
                }
                Ok(_) => {}
                Err(error) => {
                    log::debug!(
                        "ScreenCaptureKit has not exposed virtual display {} yet: {error}",
                        self.display_id
                    );
                }
            }
            if started.elapsed() >= SHAREABLE_TIMEOUT {
                bail!(
                    "temporary virtual display {} was created but did not become visible to ScreenCaptureKit; grant Screen Recording permission in System Settings",
                    self.display_id
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn stop_inner(&mut self, announce: bool) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        // Closing the lifetime pipe commits this session to shutdown. Mark it
        // before doing fallible work so Drop cannot repeat a timed-out teardown.
        self.stopped = true;
        if announce {
            println!("Removing temporary extended display {}...", self.display_id);
        }
        self.lifetime.take();

        let started = Instant::now();
        let status = loop {
            if let Some(status) = self
                .child
                .try_wait()
                .context("could not query the virtual display helper during shutdown")?
            {
                break status;
            }
            if started.elapsed() >= SHUTDOWN_TIMEOUT {
                self.child
                    .kill()
                    .context("could not terminate the virtual display helper")?;
                break self
                    .child
                    .wait()
                    .context("could not reap the virtual display helper")?;
            }
            thread::sleep(Duration::from_millis(50));
        };
        log::debug!(
            "virtual display helper for display {} exited with {status} after {:.1} ms",
            self.display_id,
            started.elapsed().as_secs_f64() * 1_000.0
        );

        let offline_started = Instant::now();
        while native_is_online(self.display_id) {
            if offline_started.elapsed() >= SHAREABLE_TIMEOUT {
                bail!(
                    "temporary virtual display {} is still online after its helper stopped with {status}",
                    self.display_id,
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
        log::debug!(
            "temporary virtual display {} went offline {:.1} ms after its helper stopped",
            self.display_id,
            offline_started.elapsed().as_secs_f64() * 1_000.0
        );
        if !status.success() {
            log::warn!(
                "virtual display helper exited with {status}, but display {} is offline",
                self.display_id
            );
        }
        if announce {
            println!("Removed temporary extended display {}.", self.display_id);
        }
        Ok(())
    }
}

impl Drop for VirtualDisplaySession {
    fn drop(&mut self) {
        if let Err(error) = self.stop_inner(false) {
            log::warn!(
                "could not clean up temporary virtual display {}: {error:#}",
                self.display_id
            );
        }
    }
}

pub fn run_helper(width: u32, height: u32, fps: u32) -> Result<()> {
    let mut error_buffer = [0 as c_char; 512];
    let display_id = native_create(width, height, fps, &mut error_buffer);
    if display_id == 0 {
        let message = unsafe { CStr::from_ptr(error_buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let message = if message.is_empty() {
            "macOS did not create the virtual display".to_owned()
        } else {
            single_line(&message)
        };
        println!("ERROR {message}");
        io::stdout().flush().ok();
        bail!(message);
    }

    let guard = NativeDisplayGuard { armed: true };
    println!("READY {display_id}");
    io::stdout()
        .flush()
        .context("could not report virtual display readiness")?;

    let read_result = (|| {
        let mut input = io::stdin().lock();
        let mut buffer = [0_u8; 64];
        loop {
            match input.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(error).context("virtual display lifetime pipe failed");
                }
            }
        }
        Ok(())
    })();

    // WindowServer may keep the display registered until this helper exits, so
    // the parent performs the offline check after reaping the process.
    guard.release();
    read_result
}

struct NativeDisplayGuard {
    armed: bool,
}

impl NativeDisplayGuard {
    fn release(mut self) {
        self.armed = false;
        native_destroy();
    }
}

impl Drop for NativeDisplayGuard {
    fn drop(&mut self) {
        if self.armed {
            native_destroy();
        }
    }
}

fn parse_startup_message(line: &str) -> Result<u32> {
    let line = line.trim();
    if let Some(id) = line.strip_prefix("READY ") {
        let id = id
            .parse::<u32>()
            .context("virtual display helper returned an invalid display ID")?;
        if id == 0 {
            bail!("virtual display helper returned display ID zero");
        }
        return Ok(id);
    }
    if let Some(message) = line.strip_prefix("ERROR ") {
        bail!("could not create a temporary virtual display: {message}");
    }
    bail!("virtual display helper returned an invalid startup response")
}

fn single_line(message: &str) -> String {
    message.replace('\r', " ").replace('\n', " ")
}

fn terminate_child(child: &mut Child) {
    match child.try_wait() {
        Ok(Some(_)) => return,
        Ok(None) | Err(_) => {}
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(target_os = "macos")]
fn native_create(width: u32, height: u32, fps: u32, error_buffer: &mut [c_char]) -> u32 {
    // SAFETY: the Objective-C bridge receives primitive values and a valid,
    // writable buffer for the duration of the call.
    unsafe {
        cast_virtual_display_create(
            width,
            height,
            fps,
            error_buffer.as_mut_ptr(),
            error_buffer.len(),
        )
    }
}

#[cfg(not(target_os = "macos"))]
fn native_create(_width: u32, _height: u32, _fps: u32, error_buffer: &mut [c_char]) -> u32 {
    let message = b"temporary virtual displays are available only on macOS\0";
    for (target, source) in error_buffer.iter_mut().zip(message.iter().copied()) {
        *target = source as c_char;
    }
    0
}

#[cfg(target_os = "macos")]
fn native_destroy() {
    // SAFETY: the helper owns at most one Cast display. The native operation is
    // idempotent and receives no borrowed data.
    unsafe { cast_virtual_display_destroy() }
}

#[cfg(not(target_os = "macos"))]
fn native_destroy() {}

#[cfg(target_os = "macos")]
fn native_is_online(display_id: u32) -> bool {
    // SAFETY: CoreGraphics accepts any display identifier for this query.
    unsafe { cast_virtual_display_is_online(display_id) }
}

#[cfg(not(target_os = "macos"))]
fn native_is_online(_display_id: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::{parse_startup_message, single_line};

    #[test]
    fn parses_ready_message() {
        assert_eq!(parse_startup_message("READY 42\n").unwrap(), 42);
    }

    #[test]
    fn rejects_zero_and_malformed_ready_messages() {
        assert!(parse_startup_message("READY 0\n").is_err());
        assert!(parse_startup_message("READY display\n").is_err());
        assert!(parse_startup_message("hello\n").is_err());
    }

    #[test]
    fn reports_native_startup_errors() {
        let error = parse_startup_message("ERROR API unavailable\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("API unavailable"));
    }

    #[test]
    fn helper_protocol_messages_stay_on_one_line() {
        assert_eq!(single_line("one\ntwo\rthree"), "one two three");
    }
}
