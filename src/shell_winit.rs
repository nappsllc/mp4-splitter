//! The window on winit and softbuffer, and the file dialogs on rfd (macOS and
//! Linux, until they are ported to hand-rolled layers as Windows is).

use crate::app::App;
use crate::shell::{Button, CursorKind, Host, Key, Mods, Picked, Wake};
use crate::theme::Theme;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WKey, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

#[derive(Debug, Clone, Copy)]
enum UserEvent {
    Wake,
}

struct WinitHost {
    window: Rc<Window>,
    proxy: EventLoopProxy<UserEvent>,
}

impl Host for WinitHost {
    fn size(&self) -> (u32, u32) {
        let s = self.window.inner_size();
        (s.width, s.height)
    }

    fn scale(&self) -> f32 {
        self.window.scale_factor() as f32
    }

    fn redraw(&self, _rows: Option<(u32, u32)>) {
        self.window.request_redraw();
    }

    fn set_title(&self, title: &str) {
        self.window.set_title(title);
    }

    fn set_cursor(&self, c: CursorKind) {
        self.window.set_cursor(match c {
            CursorKind::Default => CursorIcon::Default,
            CursorKind::Pointer => CursorIcon::Pointer,
            CursorKind::EwResize => CursorIcon::EwResize,
            CursorKind::Grabbing => CursorIcon::Grabbing,
        });
    }

    fn set_colors(&self, _t: &Theme) {}

    fn wake(&self) -> Wake {
        let p = self.proxy.clone();
        Arc::new(move || {
            let _ = p.send_event(UserEvent::Wake);
        })
    }

    fn pick_file(&self, dir: Option<PathBuf>, done: Picked) {
        let mut d = rfd::FileDialog::new().add_filter("Video", &["mp4", "m4v", "mov", "MP4", "M4V", "MOV"]).set_title("Open a video");
        if let Some(dir) = dir {
            d = d.set_directory(dir);
        }
        done(d.pick_file());
    }

    fn pick_folder(&self, dir: Option<PathBuf>, done: Picked) {
        let mut d = rfd::FileDialog::new().set_title("Export the pieces to");
        if let Some(dir) = dir {
            d = d.set_directory(dir);
        }
        done(d.pick_folder());
    }
}

struct Shell {
    proxy: EventLoopProxy<UserEvent>,
    open: Option<PathBuf>,
    app: Option<App>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    mods: Mods,
}

fn key_of(k: &WKey) -> Option<Key> {
    Some(match k {
        WKey::Named(NamedKey::Space) => Key::Space,
        WKey::Named(NamedKey::ArrowLeft) => Key::Left,
        WKey::Named(NamedKey::ArrowRight) => Key::Right,
        WKey::Named(NamedKey::ArrowUp) => Key::Up,
        WKey::Named(NamedKey::ArrowDown) => Key::Down,
        WKey::Named(NamedKey::PageUp) => Key::PageUp,
        WKey::Named(NamedKey::PageDown) => Key::PageDown,
        WKey::Named(NamedKey::Home) => Key::Home,
        WKey::Named(NamedKey::End) => Key::End,
        WKey::Named(NamedKey::Delete) => Key::Delete,
        WKey::Named(NamedKey::Backspace) => Key::Backspace,
        WKey::Named(NamedKey::Escape) => Key::Escape,
        WKey::Named(NamedKey::Enter) => Key::Enter,
        WKey::Named(NamedKey::F1) => Key::F1,
        WKey::Character(c) => Key::Char(c.to_lowercase().chars().next()?),
        _ => return None,
    })
}

impl ApplicationHandler<UserEvent> for Shell {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("MP4 Splitter")
            .with_inner_size(LogicalSize::new(1180.0, 780.0))
            .with_min_inner_size(LogicalSize::new(760.0, 560.0));
        let window = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                eprintln!("no window: {e}");
                el.exit();
                return;
            }
        };
        let ctx = match softbuffer::Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("no drawing surface: {e}");
                el.exit();
                return;
            }
        };
        self.surface = softbuffer::Surface::new(&ctx, window.clone()).ok();
        let dark = window.theme().map(|t| t == winit::window::Theme::Dark);
        let host = WinitHost { window: window.clone(), proxy: self.proxy.clone() };
        self.window = Some(window);
        self.app = Some(App::new(Box::new(host), dark, self.open.take()));
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _ev: UserEvent) {
        if let Some(a) = &mut self.app {
            a.on_wake();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        let Some(a) = &mut self.app else { return };
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => {
                let frame = a.paint();
                if let (Some(surface), Some((nw, nh))) = (&mut self.surface, NonZeroU32::new(frame.w).zip(NonZeroU32::new(frame.h))) {
                    if surface.resize(nw, nh).is_ok() {
                        if let Ok(mut buf) = surface.buffer_mut() {
                            for (o, p) in buf.iter_mut().zip(frame.bgra.chunks_exact(4)) {
                                *o = p[0] as u32 | (p[1] as u32) << 8 | (p[2] as u32) << 16;
                            }
                            let _ = buf.present();
                        }
                    }
                }
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => a.on_resize(),
            WindowEvent::ThemeChanged(th) => a.on_theme(Some(th == winit::window::Theme::Dark)),
            WindowEvent::ModifiersChanged(m) => {
                let st = m.state();
                self.mods = Mods { shift: st.shift_key(), ctrl: st.control_key() || st.super_key() };
                a.on_mods(self.mods);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let Some(k) = key_of(&event.logical_key) {
                        a.on_key(k);
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => a.on_move(position.x as f32, position.y as f32),
            WindowEvent::CursorLeft { .. } => a.on_leave(),
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    MouseButton::Left => Button::Left,
                    MouseButton::Middle => Button::Middle,
                    MouseButton::Right => Button::Right,
                    _ => return,
                };
                a.on_button(b, state == ElementState::Pressed);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x as f64, y as f64),
                    MouseScrollDelta::PixelDelta(p) => (p.x / 40.0, p.y / 40.0),
                };
                a.on_wheel(dx, dy);
            }
            WindowEvent::HoveredFile(_) => a.on_hover_file(true),
            WindowEvent::HoveredFileCancelled => a.on_hover_file(false),
            WindowEvent::DroppedFile(p) => a.on_drop(&p),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let Some(a) = &mut self.app else { return };
        let t = a.tick();
        if t.exit {
            el.exit();
            return;
        }
        el.set_control_flow(match t.wait {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        });
    }
}

/// Open the window and run until it closes.
pub fn run(open: Option<PathBuf>) -> anyhow::Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let mut shell = Shell { proxy: event_loop.create_proxy(), open, app: None, window: None, surface: None, mods: Mods::default() };
    event_loop.run_app(&mut shell)?;
    Ok(())
}
