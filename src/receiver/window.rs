use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use softbuffer::Surface;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use super::ReceiverEvent;
use super::decode::FrameSlot;
use super::platform::ReceiverCore;

/// Everything the windowed event loop drives: the frame sink, the core, the
/// lifecycle event stream, and the shutdown controls.
pub struct WindowConfig {
    pub title: String,
    pub slot: FrameSlot,
    pub core: Arc<ReceiverCore>,
    pub events: std::sync::mpsc::Receiver<ReceiverEvent>,
    pub json: bool,
    pub stop: Arc<AtomicBool>,
    pub deadline: Option<Instant>,
}

/// Wakes the event loop when a new video frame lands in the shared slot.
#[derive(Debug)]
pub struct FrameReady;

/// Runs the winit event loop on the calling thread until the window closes or
/// the receiver shuts down.
pub fn run(
    config: WindowConfig,
    event_loop: EventLoop<FrameReady>,
) -> std::sync::mpsc::Receiver<ReceiverEvent> {
    let mut handler = VideoApp {
        window: None,
        surface: None,
        slot: config.slot,
        core: config.core,
        events: Some(config.events),
        title: config.title,
        json: config.json,
        stop: config.stop,
        deadline: config.deadline,
        size: (640, 360),
    };
    if let Err(error) = event_loop.run_app(&mut handler) {
        log::warn!("the receiver video window exited with an error: {error}");
    }
    // Hand the event receiver back so shutdown-time events still drain.
    handler
        .events
        .take()
        .expect("the event receiver lives until the loop exits")
}

struct VideoApp {
    window: Option<Arc<Window>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    slot: FrameSlot,
    core: Arc<ReceiverCore>,
    events: Option<std::sync::mpsc::Receiver<ReceiverEvent>>,
    title: String,
    json: bool,
    stop: Arc<AtomicBool>,
    deadline: Option<Instant>,
    size: (u32, u32),
}

impl VideoApp {
    fn drain_events(&mut self, event_loop: &ActiveEventLoop) {
        let Some(receiver) = self.events.as_ref() else {
            return;
        };
        while let Ok(event) = receiver.try_recv() {
            if let ReceiverEvent::MediaLoading { title } = &event {
                self.title = format!("{} — {}", self.core.name(), title);
                if let Some(window) = &self.window {
                    window.set_title(&self.title);
                }
            }
            if matches!(event, ReceiverEvent::Shutdown) {
                event_loop.exit();
                return;
            }
            super::emit_event(&event, self.json);
        }
    }

    fn draw(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let frame = self.slot.lock().expect("frame slot").take();
        let Some(frame) = frame else {
            return;
        };
        let (window_width, window_height): (u32, u32) = {
            let size = window.inner_size();
            (size.width.max(1), size.height.max(1))
        };
        if (self.size.0, self.size.1) != (window_width, window_height) {
            let _ = surface.resize(
                NonZeroU32::new(window_width).expect("width is non-zero"),
                NonZeroU32::new(window_height).expect("height is non-zero"),
            );
            self.size = (window_width, window_height);
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        blit(
            &mut buffer,
            window_width,
            window_height,
            &frame.pixels,
            frame.width,
            frame.height,
        );
        if buffer.present().is_err() {
            log::debug!("video window presentation failed");
        }
    }
}

impl ApplicationHandler<FrameReady> for VideoApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attributes = Window::default_attributes()
            .with_title(&self.title)
            .with_inner_size(winit::dpi::LogicalSize::new(self.size.0, self.size.1));
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .expect("could not create the receiver video window"),
        );
        let context =
            softbuffer::Context::new(Arc::clone(&window)).expect("could not create a renderer");
        let surface = softbuffer::Surface::new(&context, Arc::clone(&window))
            .expect("could not bind the renderer to the window");
        self.window = Some(window);
        self.surface = Some(surface);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                log::info!("receiver video window closed; stopping playback");
                self.core.stop_media();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                self.size = (size.width.max(1), size.height.max(1));
                if let Some(surface) = self.surface.as_mut() {
                    let _ = surface.resize(
                        NonZeroU32::new(self.size.0).expect("width is non-zero"),
                        NonZeroU32::new(self.size.1).expect("height is non-zero"),
                    );
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::KeyboardInput { event, .. }
                // Space toggles pause; Escape stops the current media.
                if event.state == ElementState::Pressed => {
                    match event.logical_key.as_ref() {
                        Key::Named(NamedKey::Space) => self.core.toggle_pause(),
                        Key::Named(NamedKey::Escape) => {
                            self.core.stop_media();
                            event_loop.exit();
                        }
                        _ => {}
                    }
                }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.drain_events(event_loop);
        if self.stop.load(Ordering::SeqCst)
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            event_loop.exit();
            return;
        }
        if self.core.has_pending_frame()
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
    }
}

/// Nearest-neighbour scale-blit of a source frame into the window buffer,
/// repacking 0x00BBGGRR source pixels into softbuffer's 0x00RRGGBB layout.
fn blit(
    buffer: &mut [u32],
    dst_width: u32,
    dst_height: u32,
    src: &[u32],
    src_width: u32,
    src_height: u32,
) {
    if src_width == 0 || src_height == 0 || src.is_empty() || buffer.is_empty() {
        return;
    }
    let dst_width = dst_width as usize;
    let dst_height = dst_height as usize;
    let src_width = src_width as usize;
    let src_height = src_height as usize;
    for row in 0..dst_height {
        let source_row = row * src_height / dst_height;
        let target_row = row * dst_width;
        for column in 0..dst_width {
            let source_column = column * src_width / dst_width;
            let pixel = src[source_row * src_width + source_column];
            let red = pixel & 0xFF;
            let green = (pixel >> 8) & 0xFF;
            let blue = (pixel >> 16) & 0xFF;
            buffer[target_row + column] = (red << 16) | (green << 8) | blue;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_pixel(channels: [u8; 4]) -> u32 {
        u32::from_le_bytes(channels)
    }

    #[test]
    fn blit_scales_and_repacks_pixel_order() {
        // Source is 2x1: pure red then pure blue, in ffmpeg RGBZ byte order
        // (red in the lowest byte).
        let source = vec![
            source_pixel([255, 0, 0, 255]),
            source_pixel([0, 0, 255, 255]),
        ];
        let mut buffer = vec![0_u32; 4];
        blit(&mut buffer, 4, 1, &source, 2, 1);
        // Each source pixel is doubled into softbuffer's 0x00RRGGBB layout.
        assert_eq!(buffer[0], 0x00FF_0000);
        assert_eq!(buffer[1], 0x00FF_0000);
        assert_eq!(buffer[2], 0x0000_00FF);
        assert_eq!(buffer[3], 0x0000_00FF);
    }

    #[test]
    fn blit_handles_identity_size() {
        let source = vec![
            source_pixel([0, 0, 255, 255]),
            source_pixel([0, 255, 0, 255]),
        ];
        let mut buffer = vec![0_u32; 2];
        blit(&mut buffer, 2, 1, &source, 2, 1);
        assert_eq!(buffer[0], 0x0000_00FF);
        assert_eq!(buffer[1], 0x0000_FF00);
    }

    #[test]
    fn blit_scales_down_without_panicking() {
        let source = vec![source_pixel([255, 0, 0, 255]); 4];
        let mut buffer = vec![0_u32; 2];
        blit(&mut buffer, 2, 1, &source, 4, 1);
        assert_eq!(buffer.len(), 2);
        assert!(buffer.iter().all(|pixel| *pixel == 0x00FF_0000));
    }
}
