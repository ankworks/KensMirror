// SPDX-License-Identifier: GPL-3.0-or-later
//! The window: shows the newest picture, scaled to fit.
//!
//! Keys: R starts and stops recording, F11 or F toggles fullscreen, Esc leaves it,
//! T toggles always-on-top.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId, WindowLevel};

use crate::engine::{FrameSlot, UiEvent};
use crate::paint::{BACKGROUND, Scaler};
use crate::recorder::Recorder;

const APP: &str = "KensMirror";

struct App {
    slot: Arc<FrameSlot>,
    recorder: Arc<Recorder>,
    name: String,
    window: Option<Rc<Window>>,
    context: Option<softbuffer::Context<Rc<Window>>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    scaler: Scaler,
    video_size: Option<(u32, u32)>,
    /// The slot generation that was drawn last.
    painted: u64,
    on_top: bool,
    problem: Option<String>,
    failure: Option<String>,
}

impl App {
    fn update_title(&self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let mut title = if let Some(problem) = &self.problem {
            format!("{APP} - error: {problem}")
        } else if let Some((width, height)) = self.video_size {
            format!("{APP} - {width}x{height}")
        } else {
            format!(
                "{APP} - waiting - pick \"{}\" under Screen Mirroring on your device",
                self.name
            )
        };
        if self.recorder.is_active() {
            title.push_str(" - REC (R to stop)");
        } else if let Some(note) = self.recorder.note() {
            title.push_str(" - ");
            title.push_str(&note);
        }
        window.set_title(&title);
    }

    /// Sizes the window to the picture, without growing past the screen.
    fn fit_window(&self, width: u32, height: u32) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        if window.fullscreen().is_some() || window.is_maximized() {
            return;
        }
        let (limit_w, limit_h) = match window.current_monitor() {
            Some(monitor) => {
                let size = monitor.size();
                (size.width as f64 * 0.90, size.height as f64 * 0.82)
            }
            None => (1200.0, 620.0),
        };
        let scale = (limit_w / width as f64)
            .min(limit_h / height as f64)
            .min(1.0);
        let target = PhysicalSize::new(
            ((width as f64 * scale).round() as u32).max(160),
            ((height as f64 * scale).round() as u32).max(160),
        );
        let _ = window.request_inner_size(target);
    }

    fn paint(&mut self) {
        self.painted = self.slot.generation.load(Ordering::Acquire);
        let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_mut()) else {
            return;
        };
        let size = window.inner_size();
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };
        if let Err(error) = surface.resize(width, height) {
            log::warn!("window buffer resize failed: {error}");
            return;
        }
        let mut buffer = match surface.buffer_mut() {
            Ok(buffer) => buffer,
            Err(error) => {
                log::warn!("window buffer unavailable: {error}");
                return;
            }
        };
        let frame = self.slot.frame.lock().expect("the frame slot").clone();
        match frame {
            Some(frame) => self.scaler.draw(
                &frame,
                &mut buffer,
                size.width as usize,
                size.height as usize,
            ),
            None => buffer.fill(BACKGROUND),
        }
        if let Err(error) = buffer.present() {
            log::warn!("window present failed: {error}");
        }
    }
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(APP)
            .with_inner_size(LogicalSize::new(405.0, 720.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Rc::new(window),
            Err(error) => {
                self.failure = Some(format!("could not create the window: {error}"));
                event_loop.exit();
                return;
            }
        };
        let context = match softbuffer::Context::new(Rc::clone(&window)) {
            Ok(context) => context,
            Err(error) => {
                self.failure = Some(format!("could not open the display: {error}"));
                event_loop.exit();
                return;
            }
        };
        let surface = match softbuffer::Surface::new(&context, Rc::clone(&window)) {
            Ok(surface) => surface,
            Err(error) => {
                self.failure = Some(format!("could not create the window buffer: {error}"));
                event_loop.exit();
                return;
            }
        };
        self.window = Some(window);
        self.context = Some(context);
        self.surface = Some(surface);
        self.update_title();
        log::info!("window ready");
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::Frame => {
                // Several of these can queue up while one picture is drawn: only the
                // newest picture is worth drawing, and it is drawn at once.
                if self.slot.generation.load(Ordering::Acquire) == self.painted {
                    return;
                }
                let size = self
                    .slot
                    .frame
                    .lock()
                    .expect("the frame slot")
                    .as_ref()
                    .map(|frame| (frame.width, frame.height));
                if let Some((width, height)) = size {
                    if self.video_size != Some((width, height)) {
                        log::info!("picture size {width}x{height}");
                        self.video_size = Some((width, height));
                        self.fit_window(width, height);
                        self.update_title();
                    }
                }
                self.paint();
            }
            UiEvent::VideoClosed => {
                self.video_size = None;
                self.update_title();
                self.paint();
            }
            UiEvent::Recorder => self.update_title(),
            UiEvent::ServeEnded(None) => event_loop.exit(),
            UiEvent::ServeEnded(Some(problem)) => {
                self.problem = Some(problem);
                self.update_title();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.paint(),
            WindowEvent::Resized(_) => {
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed || event.repeat {
                    return;
                }
                let Some(window) = self.window.as_ref() else {
                    return;
                };
                let toggle_fullscreen = |window: &Window| {
                    if window.fullscreen().is_some() {
                        window.set_fullscreen(None);
                    } else {
                        window.set_fullscreen(Some(Fullscreen::Borderless(None)));
                    }
                };
                match &event.logical_key {
                    Key::Named(NamedKey::F11) => toggle_fullscreen(window),
                    Key::Named(NamedKey::Escape) => window.set_fullscreen(None),
                    Key::Character(text) if text.eq_ignore_ascii_case("f") => {
                        toggle_fullscreen(window)
                    }
                    Key::Character(text) if text.eq_ignore_ascii_case("t") => {
                        self.on_top = !self.on_top;
                        window.set_window_level(if self.on_top {
                            WindowLevel::AlwaysOnTop
                        } else {
                            WindowLevel::Normal
                        });
                    }
                    Key::Character(text) if text.eq_ignore_ascii_case("r") => {
                        self.recorder.toggle();
                        self.update_title();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Runs the window until it is closed or the receiver stops.
pub fn run(
    event_loop: EventLoop<UiEvent>,
    slot: Arc<FrameSlot>,
    recorder: Arc<Recorder>,
    name: String,
) -> Result<(), String> {
    let mut app = App {
        slot,
        recorder,
        name,
        window: None,
        context: None,
        surface: None,
        scaler: Scaler::default(),
        video_size: None,
        painted: 0,
        on_top: false,
        problem: None,
        failure: None,
    };
    event_loop
        .run_app(&mut app)
        .map_err(|error| error.to_string())?;
    // The buffer must go before the window it draws into.
    app.surface = None;
    app.context = None;
    match app.failure.take() {
        Some(failure) => Err(failure),
        None => Ok(()),
    }
}
