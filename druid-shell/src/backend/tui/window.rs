// Copyright 2026 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! Terminal window creation, rendering, and event dispatch.

use std::cell::{Cell, RefCell};
use std::io::stdout;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use instant::Instant;
use ratatui::crossterm::event::{
    Event as TerminalEvent, KeyCode, KeyEvent as TerminalKeyEvent, KeyEventKind, KeyModifiers,
    MediaKeyCode, ModifierKeyCode, MouseButton as TerminalMouseButton,
    MouseEvent as TerminalMouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::SetTitle;
use ratatui::Frame;

use super::application::Application;
use super::error::Error;
use super::{config, logical_size, text_factory, CELL_HEIGHT, CELL_WIDTH};
use crate::common_util::{ClickCounter, IdleCallback};
use crate::dialog::FileDialogOptions;
use crate::error::Error as ShellError;
use crate::keyboard::{Code, KbKey, KeyEvent, KeyState, Location, Modifiers};
use crate::kurbo::{Insets, Point, Rect, Size, Vec2};
use crate::mouse::{Cursor, CursorDesc, MouseButton, MouseButtons, MouseEvent};
use crate::piet::{self, PietText};
use crate::region::Region;
use crate::scale::Scale;
use crate::text::{simulate_input, Event};
use crate::window::{
    self, FileDialogToken, IdleToken, TextFieldToken, TimerToken, WinHandler, WindowLevel,
};

use super::menu::Menu;

const WHEEL_DELTA: f64 = 120.0;
pub(crate) struct WindowState {
    handler: RefCell<Option<Box<dyn WinHandler>>>,
    size: Cell<Size>,
    position: Cell<Point>,
    window_state: Cell<window::WindowState>,
    visible: Cell<bool>,
    closed: Cell<bool>,
    destroyed: Cell<bool>,
    redraw_requested: Cell<bool>,
    anim_requested: Cell<bool>,
    invalid: RefCell<Region>,
    idle_queue: Arc<Mutex<Vec<IdleKind>>>,
    idle_wake: Arc<AtomicBool>,
    timers: Arc<Mutex<Vec<(Instant, TimerToken)>>>,
    active_text_input: Cell<Option<TextFieldToken>>,
    mouse_buttons: Cell<MouseButtons>,
    click_counter: ClickCounter,
    cursor: RefCell<Option<Cursor>>,
    title: RefCell<String>,
    text: PietText,
}

#[derive(Clone, Default)]
pub struct WindowHandle {
    state: Option<Weak<WindowState>>,
}

impl PartialEq for WindowHandle {
    fn eq(&self, other: &Self) -> bool {
        match (&self.state, &other.state) {
            (Some(left), Some(right)) => Weak::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        }
    }
}

impl Eq for WindowHandle {}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CustomCursor;

#[derive(Clone)]
pub struct IdleHandle {
    queue: Arc<Mutex<Vec<IdleKind>>>,
    wake: Arc<AtomicBool>,
}

enum IdleKind {
    Callback(Box<dyn IdleCallback>),
    Token(IdleToken),
}

pub(crate) struct WindowBuilder {
    app: Application,
    handler: Option<Box<dyn WinHandler>>,
    size: Size,
    title: String,
}

impl WindowBuilder {
    pub fn new(app: Application) -> WindowBuilder {
        WindowBuilder {
            app,
            handler: None,
            size: Size::new(500.0, 400.0),
            title: String::new(),
        }
    }

    pub fn set_handler(&mut self, handler: Box<dyn WinHandler>) {
        self.handler = Some(handler);
    }

    pub fn set_size(&mut self, size: Size) {
        self.size = size;
    }

    pub fn set_min_size(&mut self, _size: Size) {}

    pub fn resizable(&mut self, _resizable: bool) {}

    pub fn show_titlebar(&mut self, _show_titlebar: bool) {}

    pub fn set_always_on_top(&mut self, _always_on_top: bool) {}

    pub fn set_transparent(&mut self, _transparent: bool) {}

    pub fn set_position(&mut self, _position: Point) {}

    pub fn set_level(&mut self, _level: WindowLevel) {}

    pub fn set_window_state(&mut self, _state: window::WindowState) {}

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    pub fn set_menu(&mut self, _menu: Menu) {}

    pub fn build(self) -> Result<WindowHandle, Error> {
        let handler = self
            .handler
            .ok_or_else(|| Error::unsupported("window built without a handler"))?;

        if !self.app.state.borrow().windows.is_empty() {
            return Err(Error::unsupported("only one window is supported at a time"));
        }

        let idle_queue = Arc::new(Mutex::new(Vec::new()));
        let idle_wake = Arc::new(AtomicBool::new(false));
        let state = Rc::new(WindowState {
            handler: RefCell::new(Some(handler)),
            size: Cell::new(self.size),
            position: Cell::new(Point::ZERO),
            window_state: Cell::new(window::WindowState::Restored),
            visible: Cell::new(false),
            closed: Cell::new(false),
            destroyed: Cell::new(false),
            redraw_requested: Cell::new(true),
            anim_requested: Cell::new(false),
            invalid: RefCell::new(Region::EMPTY),
            idle_queue,
            idle_wake,
            timers: Arc::new(Mutex::new(Vec::new())),
            active_text_input: Cell::new(None),
            mouse_buttons: Cell::new(MouseButtons::new()),
            click_counter: ClickCounter::default(),
            cursor: RefCell::new(None),
            title: RefCell::new(self.title),
            text: text_factory(),
        });

        let handle = WindowHandle {
            state: Some(Rc::downgrade(&state)),
        };
        let public_handle: crate::WindowHandle = handle.clone().into();
        state.with_handler(|handler| {
            handler.connect(&public_handle);
            handler.scale(Scale::new(1.0, 1.0));
            handler.size(self.size);
        });

        self.app.state.borrow_mut().windows.push(state);
        Ok(handle)
    }
}

impl WindowState {
    fn with_handler<T>(&self, callback: impl FnOnce(&mut dyn WinHandler) -> T) -> Option<T> {
        let mut handler = self.handler.try_borrow_mut().ok()?;
        let handler = handler.as_deref_mut()?;
        Some(callback(handler))
    }

    pub(crate) fn show(&self) {
        if !self.closed.get() {
            self.visible.set(true);
            self.invalidate();
        }
    }

    pub(crate) fn hide(&self) {
        self.visible.set(false);
    }

    pub(crate) fn close(&self) {
        self.closed.set(true);
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.get()
    }

    pub(crate) fn destroy(&self) {
        if !self.destroyed.replace(true) {
            self.with_handler(|handler| handler.destroy());
        }
        if let Ok(mut handler) = self.handler.try_borrow_mut() {
            handler.take();
        }
    }

    pub(crate) fn focus(&self) {
        self.with_handler(|handler| handler.got_focus());
    }

    pub(crate) fn apply_title(&self) {
        let title = self.title.borrow().clone();
        let _ = execute!(stdout(), SetTitle(title));
    }

    pub(crate) fn should_draw(&self) -> bool {
        self.visible.get() && !self.closed.get() && self.redraw_requested.get()
    }

    pub(crate) fn has_pending_work(&self) -> bool {
        self.redraw_requested.get()
            || self.idle_wake.load(Ordering::Relaxed)
            || !self
                .idle_queue
                .lock()
                .map(|queue| queue.is_empty())
                .unwrap_or(true)
            || self
                .timers
                .lock()
                .map(|timers| {
                    timers
                        .iter()
                        .any(|(deadline, _)| *deadline <= Instant::now())
                })
                .unwrap_or(false)
    }

    pub(crate) fn set_terminal_size(&self, cols: u16, rows: u16) {
        let size = logical_size(cols.max(1), rows.max(1));
        if self.size.get() != size {
            self.size.set(size);
            self.with_handler(|handler| handler.size(size));
            self.invalidate();
        }
    }

    pub(crate) fn paint(&self, frame: &mut Frame<'_>) {
        if !self.should_draw() {
            return;
        }

        let area = frame.area();
        self.set_terminal_size(area.width, area.height);
        let size = self.size.get();
        let invalid = Region::from(Rect::new(0.0, 0.0, size.width, size.height));

        self.redraw_requested.set(false);
        self.anim_requested.set(false);
        self.with_handler(|handler| handler.prepare_paint());
        let text_cursor = self.with_handler(|handler| {
            piet::render_with(frame.buffer_mut(), area, config(), |piet| {
                handler.paint(piet, &invalid);
            });
            handler.text_cursor_position()
        });

        if let Some(Some(position)) = text_cursor {
            let cfg = config();
            let col = (position.x / cfg.cell_width()).round();
            let row = (position.y / cfg.cell_height()).round();
            let right = area.x.saturating_add(area.width);
            let bottom = area.y.saturating_add(area.height);
            if col >= 0.0 && row >= 0.0 {
                let col = area.x.saturating_add(col as u16);
                let row = area.y.saturating_add(row as u16);
                if col < right && row < bottom {
                    frame.set_cursor_position((col, row));
                }
            }
        }

        self.invalid.borrow_mut().clear();
        self.redraw_requested
            .set(self.anim_requested.replace(false));
    }

    pub(crate) fn invalidate(&self) {
        let size = self.size.get();
        self.invalidate_rect(Rect::new(0.0, 0.0, size.width, size.height));
    }

    pub(crate) fn invalidate_rect(&self, rect: Rect) {
        if let Ok(mut invalid) = self.invalid.try_borrow_mut() {
            invalid.add_rect(rect);
        }
        self.redraw_requested.set(true);
    }

    pub(crate) fn request_anim_frame(&self) {
        self.anim_requested.set(true);
        self.redraw_requested.set(true);
    }

    pub(crate) fn request_timer(&self, deadline: Instant) -> TimerToken {
        let token = TimerToken::next();
        if let Ok(mut timers) = self.timers.lock() {
            timers.push((deadline, token));
        }
        token
    }

    pub(crate) fn process_timers(&self) {
        let mut due = Vec::new();
        if let Ok(mut timers) = self.timers.lock() {
            let now = Instant::now();
            let mut index = 0;
            while index < timers.len() {
                if timers[index].0 <= now {
                    due.push(timers.remove(index).1);
                } else {
                    index += 1;
                }
            }
        }
        for token in due {
            self.with_handler(|handler| handler.timer(token));
        }
    }

    pub(crate) fn process_idle(&self) {
        let callbacks = self
            .idle_queue
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default();
        self.idle_wake.store(false, Ordering::Relaxed);
        if callbacks.is_empty() {
            return;
        }

        self.with_handler(|handler| {
            for callback in callbacks {
                match callback {
                    IdleKind::Callback(callback) => callback.call(handler),
                    IdleKind::Token(token) => handler.idle(token),
                }
            }
        });
    }

    pub(crate) fn handle_event(&self, event: TerminalEvent) {
        match event {
            TerminalEvent::Key(event) => self.handle_key_event(event),
            TerminalEvent::Mouse(event) => self.handle_mouse_event(event),
            TerminalEvent::Resize(cols, rows) => self.set_terminal_size(cols, rows),
            TerminalEvent::FocusGained => {
                self.with_handler(|handler| handler.got_focus());
            }
            TerminalEvent::FocusLost => {
                self.with_handler(|handler| handler.lost_focus());
            }
            TerminalEvent::Paste(text) => {
                let event = KeyEvent {
                    state: KeyState::Down,
                    key: KbKey::Character(text),
                    code: Code::Unidentified,
                    location: Location::Standard,
                    mods: Modifiers::default(),
                    repeat: false,
                    is_composing: false,
                };
                self.with_handler(|handler| {
                    simulate_input(handler, self.active_text_input.get(), event)
                });
            }
        }
    }

    fn handle_key_event(&self, event: TerminalKeyEvent) {
        match event.kind {
            KeyEventKind::Press | KeyEventKind::Repeat => {
                let quit_key = event.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(event.code, KeyCode::Char('c' | 'q'));
                if quit_key {
                    self.with_handler(|handler| handler.request_close());
                    return;
                }

                let key_event =
                    terminal_key_event(event, KeyState::Down, event.kind == KeyEventKind::Repeat);
                self.with_handler(|handler| {
                    simulate_input(handler, self.active_text_input.get(), key_event)
                });
            }
            KeyEventKind::Release => {
                let key_event = terminal_key_event(event, KeyState::Up, false);
                self.with_handler(|handler| handler.key_up(key_event));
            }
        }
    }

    fn handle_mouse_event(&self, event: TerminalMouseEvent) {
        let position = Point::new(
            (f64::from(event.column) + 0.5) * CELL_WIDTH,
            (f64::from(event.row) + 0.5) * CELL_HEIGHT,
        );
        let mods = terminal_modifiers(event.modifiers);
        let mut buttons = self.mouse_buttons.get();

        match event.kind {
            MouseEventKind::Down(button) => {
                let button = mouse_button(button);
                buttons.insert(button);
                self.mouse_buttons.set(buttons);
                let count = self.click_counter.count_for_click(position);
                let mouse_event = MouseEvent {
                    pos: position,
                    buttons,
                    mods,
                    count,
                    focus: false,
                    button,
                    wheel_delta: Vec2::ZERO,
                };
                self.with_handler(|handler| handler.mouse_down(&mouse_event));
            }
            MouseEventKind::Up(button) => {
                let button = mouse_button(button);
                buttons.remove(button);
                self.mouse_buttons.set(buttons);
                let mouse_event = MouseEvent {
                    pos: position,
                    buttons,
                    mods,
                    count: 0,
                    focus: false,
                    button,
                    wheel_delta: Vec2::ZERO,
                };
                self.with_handler(|handler| handler.mouse_up(&mouse_event));
            }
            MouseEventKind::Drag(button) => {
                buttons.insert(mouse_button(button));
                self.mouse_buttons.set(buttons);
                let mouse_event = MouseEvent {
                    pos: position,
                    buttons,
                    mods,
                    count: 0,
                    focus: false,
                    button: MouseButton::None,
                    wheel_delta: Vec2::ZERO,
                };
                self.with_handler(|handler| handler.mouse_move(&mouse_event));
            }
            MouseEventKind::Moved => {
                let mouse_event = MouseEvent {
                    pos: position,
                    buttons,
                    mods,
                    count: 0,
                    focus: false,
                    button: MouseButton::None,
                    wheel_delta: Vec2::ZERO,
                };
                self.with_handler(|handler| handler.mouse_move(&mouse_event));
            }
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {
                let wheel_delta = match event.kind {
                    MouseEventKind::ScrollUp if event.modifiers.contains(KeyModifiers::SHIFT) => {
                        Vec2::new(-WHEEL_DELTA, 0.0)
                    }
                    MouseEventKind::ScrollUp => Vec2::new(0.0, -WHEEL_DELTA),
                    MouseEventKind::ScrollDown if event.modifiers.contains(KeyModifiers::SHIFT) => {
                        Vec2::new(WHEEL_DELTA, 0.0)
                    }
                    MouseEventKind::ScrollDown => Vec2::new(0.0, WHEEL_DELTA),
                    MouseEventKind::ScrollLeft => Vec2::new(-WHEEL_DELTA, 0.0),
                    MouseEventKind::ScrollRight => Vec2::new(WHEEL_DELTA, 0.0),
                    _ => Vec2::ZERO,
                };
                let mouse_event = MouseEvent {
                    pos: position,
                    buttons,
                    mods,
                    count: 0,
                    focus: false,
                    button: MouseButton::None,
                    wheel_delta,
                };
                self.with_handler(|handler| handler.wheel(&mouse_event));
            }
        }
    }
}

impl WindowHandle {
    fn with_state<T>(&self, callback: impl FnOnce(&WindowState) -> T) -> Option<T> {
        self.state.as_ref()?.upgrade().map(|state| callback(&state))
    }

    fn with_state_mut<T>(&self, callback: impl FnOnce(&WindowState) -> T) -> Option<T> {
        self.with_state(callback)
    }

    pub fn show(&self) {
        self.with_state(WindowState::show);
    }

    pub fn close(&self) {
        self.with_state(WindowState::close);
    }

    pub fn hide(&self) {
        self.with_state(WindowState::hide);
    }

    pub fn resizable(&self, _resizable: bool) {}

    pub fn set_window_state(&mut self, state: window::WindowState) {
        self.with_state_mut(|window| window.window_state.set(state));
    }

    pub fn get_window_state(&self) -> window::WindowState {
        self.with_state(|window| window.window_state.get())
            .unwrap_or(window::WindowState::Restored)
    }

    pub fn handle_titlebar(&self, _val: bool) {}

    pub fn show_titlebar(&self, _show_titlebar: bool) {}

    pub fn set_position(&self, position: Point) {
        self.with_state_mut(|window| window.position.set(position));
    }

    pub fn set_always_on_top(&self, _always_on_top: bool) {}

    pub fn set_mouse_pass_through(&self, _mouse_pass_through: bool) {}

    pub fn set_input_region(&self, _region: Option<Region>) {}

    pub fn is_foreground_window(&self) -> bool {
        self.with_state(|window| window.visible.get() && !window.closed.get())
            .unwrap_or(false)
    }

    pub fn get_position(&self) -> Point {
        self.with_state(|window| window.position.get())
            .unwrap_or(Point::ZERO)
    }

    pub fn content_insets(&self) -> Insets {
        Insets::ZERO
    }

    pub fn set_size(&self, size: Size) {
        self.with_state_mut(|window| {
            window.size.set(size);
            window.invalidate();
        });
    }

    pub fn get_size(&self) -> Size {
        self.with_state(|window| window.size.get())
            .unwrap_or(Size::ZERO)
    }

    pub fn bring_to_front_and_focus(&self) {
        self.with_state(|window| {
            window.show();
            window.focus();
        });
    }

    pub fn request_anim_frame(&self) {
        self.with_state_mut(WindowState::request_anim_frame);
    }

    pub fn invalidate(&self) {
        self.with_state_mut(WindowState::invalidate);
    }

    pub fn invalidate_rect(&self, rect: Rect) {
        self.with_state_mut(|window| window.invalidate_rect(rect));
    }

    pub fn set_title(&self, title: impl Into<String>) {
        let title = title.into();
        self.with_state_mut(|window| {
            *window.title.borrow_mut() = title;
            window.apply_title();
        });
    }

    pub fn set_menu(&self, _menu: Menu) {}

    pub fn text(&self) -> PietText {
        self.with_state(|window| window.text.clone())
            .unwrap_or_else(text_factory)
    }

    pub fn add_text_field(&self) -> TextFieldToken {
        TextFieldToken::next()
    }

    pub fn remove_text_field(&self, token: TextFieldToken) {
        self.with_state_mut(|window| {
            if window.active_text_input.get() == Some(token) {
                window.active_text_input.set(None);
            }
        });
    }

    pub fn set_focused_text_field(&self, active_field: Option<TextFieldToken>) {
        self.with_state_mut(|window| window.active_text_input.set(active_field));
    }

    pub fn update_text_field(&self, _token: TextFieldToken, _update: Event) {}

    pub fn request_timer(&self, deadline: Instant) -> TimerToken {
        self.with_state(|window| window.request_timer(deadline))
            .unwrap_or(TimerToken::INVALID)
    }

    pub fn set_cursor(&mut self, cursor: &Cursor) {
        self.with_state_mut(|window| *window.cursor.borrow_mut() = Some(cursor.clone()));
    }

    pub fn make_cursor(&self, _desc: &CursorDesc) -> Option<Cursor> {
        None
    }

    pub fn open_file(&mut self, _options: FileDialogOptions) -> Option<FileDialogToken> {
        tracing::warn!("file dialogs are not supported by the TUI backend");
        None
    }

    pub fn save_as(&mut self, _options: FileDialogOptions) -> Option<FileDialogToken> {
        tracing::warn!("file dialogs are not supported by the TUI backend");
        None
    }

    pub fn show_context_menu(&self, _menu: Menu, _pos: Point) {
        tracing::warn!("context menus are not supported by the TUI backend");
    }

    pub fn get_idle_handle(&self) -> Option<IdleHandle> {
        self.with_state(|window| IdleHandle {
            queue: window.idle_queue.clone(),
            wake: window.idle_wake.clone(),
        })
    }

    pub fn get_scale(&self) -> Result<Scale, ShellError> {
        self.state
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|_| Scale::new(1.0, 1.0))
            .ok_or(ShellError::WindowDropped)
    }
}

impl IdleHandle {
    pub fn add_idle_callback<F>(&self, callback: F)
    where
        F: FnOnce(&mut dyn WinHandler) + Send + 'static,
    {
        if let Ok(mut queue) = self.queue.lock() {
            queue.push(IdleKind::Callback(Box::new(callback)));
            self.wake.store(true, Ordering::Relaxed);
        }
    }

    pub fn add_idle_token(&self, token: IdleToken) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.push(IdleKind::Token(token));
            self.wake.store(true, Ordering::Relaxed);
        }
    }
}

fn terminal_modifiers(modifiers: KeyModifiers) -> Modifiers {
    let mut result = Modifiers::default();
    result.set(Modifiers::SHIFT, modifiers.contains(KeyModifiers::SHIFT));
    result.set(
        Modifiers::CONTROL,
        modifiers.contains(KeyModifiers::CONTROL),
    );
    result.set(Modifiers::ALT, modifiers.contains(KeyModifiers::ALT));
    result.set(
        Modifiers::META,
        modifiers.contains(KeyModifiers::SUPER) || modifiers.contains(KeyModifiers::META),
    );
    result.set(Modifiers::HYPER, modifiers.contains(KeyModifiers::HYPER));
    result
}

fn terminal_key_event(event: TerminalKeyEvent, state: KeyState, repeat: bool) -> KeyEvent {
    KeyEvent {
        state,
        key: terminal_key(event.code),
        code: terminal_code(event.code),
        location: Location::Standard,
        mods: terminal_modifiers(event.modifiers),
        repeat,
        is_composing: false,
    }
}

fn terminal_key(code: KeyCode) -> KbKey {
    match code {
        KeyCode::Backspace => KbKey::Backspace,
        KeyCode::Enter => KbKey::Enter,
        KeyCode::Left => KbKey::ArrowLeft,
        KeyCode::Right => KbKey::ArrowRight,
        KeyCode::Up => KbKey::ArrowUp,
        KeyCode::Down => KbKey::ArrowDown,
        KeyCode::Home => KbKey::Home,
        KeyCode::End => KbKey::End,
        KeyCode::PageUp => KbKey::PageUp,
        KeyCode::PageDown => KbKey::PageDown,
        KeyCode::Tab | KeyCode::BackTab => KbKey::Tab,
        KeyCode::Delete => KbKey::Delete,
        KeyCode::Insert => KbKey::Insert,
        KeyCode::Esc => KbKey::Escape,
        KeyCode::Char(character) => KbKey::Character(character.to_string()),
        KeyCode::Null => KbKey::Unidentified,
        KeyCode::CapsLock => KbKey::CapsLock,
        KeyCode::ScrollLock => KbKey::ScrollLock,
        KeyCode::NumLock => KbKey::NumLock,
        KeyCode::PrintScreen => KbKey::PrintScreen,
        KeyCode::Pause => KbKey::Pause,
        KeyCode::Menu => KbKey::ContextMenu,
        KeyCode::KeypadBegin => KbKey::Clear,
        KeyCode::F(number) => function_key(number),
        KeyCode::Media(media) => media_key(media),
        KeyCode::Modifier(modifier) => modifier_key(modifier),
    }
}

fn function_key(number: u8) -> KbKey {
    match number {
        1 => KbKey::F1,
        2 => KbKey::F2,
        3 => KbKey::F3,
        4 => KbKey::F4,
        5 => KbKey::F5,
        6 => KbKey::F6,
        7 => KbKey::F7,
        8 => KbKey::F8,
        9 => KbKey::F9,
        10 => KbKey::F10,
        11 => KbKey::F11,
        12 => KbKey::F12,
        13 => KbKey::F13,
        14 => KbKey::F14,
        15 => KbKey::F15,
        16 => KbKey::F16,
        17 => KbKey::F17,
        18 => KbKey::F18,
        19 => KbKey::F19,
        20 => KbKey::F20,
        21 => KbKey::F21,
        22 => KbKey::F22,
        23 => KbKey::F23,
        24 => KbKey::F24,
        _ => KbKey::Unidentified,
    }
}

fn media_key(key: MediaKeyCode) -> KbKey {
    match key {
        MediaKeyCode::Play => KbKey::MediaPlay,
        MediaKeyCode::Pause => KbKey::MediaPause,
        MediaKeyCode::PlayPause => KbKey::MediaPlayPause,
        MediaKeyCode::Reverse => KbKey::MediaTrackPrevious,
        MediaKeyCode::Stop => KbKey::MediaStop,
        MediaKeyCode::FastForward => KbKey::MediaTrackNext,
        MediaKeyCode::Rewind => KbKey::MediaTrackPrevious,
        MediaKeyCode::TrackNext => KbKey::MediaTrackNext,
        MediaKeyCode::TrackPrevious => KbKey::MediaTrackPrevious,
        MediaKeyCode::Record => KbKey::MediaRecord,
        MediaKeyCode::LowerVolume => KbKey::AudioVolumeDown,
        MediaKeyCode::RaiseVolume => KbKey::AudioVolumeUp,
        MediaKeyCode::MuteVolume => KbKey::AudioVolumeMute,
    }
}

fn modifier_key(key: ModifierKeyCode) -> KbKey {
    match key {
        ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift => KbKey::Shift,
        ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl => KbKey::Control,
        ModifierKeyCode::LeftAlt | ModifierKeyCode::RightAlt => KbKey::Alt,
        ModifierKeyCode::LeftSuper
        | ModifierKeyCode::RightSuper
        | ModifierKeyCode::LeftMeta
        | ModifierKeyCode::RightMeta => KbKey::Meta,
        ModifierKeyCode::LeftHyper | ModifierKeyCode::RightHyper => KbKey::Hyper,
        ModifierKeyCode::IsoLevel3Shift => KbKey::AltGraph,
        ModifierKeyCode::IsoLevel5Shift => KbKey::Unidentified,
    }
}

fn terminal_code(code: KeyCode) -> Code {
    match code {
        KeyCode::Backspace => Code::Backspace,
        KeyCode::Enter => Code::Enter,
        KeyCode::Left => Code::ArrowLeft,
        KeyCode::Right => Code::ArrowRight,
        KeyCode::Up => Code::ArrowUp,
        KeyCode::Down => Code::ArrowDown,
        KeyCode::Home => Code::Home,
        KeyCode::End => Code::End,
        KeyCode::PageUp => Code::PageUp,
        KeyCode::PageDown => Code::PageDown,
        KeyCode::Tab => Code::Tab,
        KeyCode::BackTab => Code::Tab,
        KeyCode::Delete => Code::Delete,
        KeyCode::Insert => Code::Insert,
        KeyCode::Esc => Code::Escape,
        KeyCode::CapsLock => Code::CapsLock,
        KeyCode::ScrollLock => Code::ScrollLock,
        KeyCode::NumLock => Code::NumLock,
        KeyCode::PrintScreen => Code::PrintScreen,
        KeyCode::Pause => Code::Pause,
        KeyCode::F(number) => function_code(number),
        KeyCode::Char(character) => character_code(character),
        _ => Code::Unidentified,
    }
}

fn function_code(number: u8) -> Code {
    match number {
        1 => Code::F1,
        2 => Code::F2,
        3 => Code::F3,
        4 => Code::F4,
        5 => Code::F5,
        6 => Code::F6,
        7 => Code::F7,
        8 => Code::F8,
        9 => Code::F9,
        10 => Code::F10,
        11 => Code::F11,
        12 => Code::F12,
        13 => Code::F13,
        14 => Code::F14,
        15 => Code::F15,
        16 => Code::F16,
        17 => Code::F17,
        18 => Code::F18,
        19 => Code::F19,
        20 => Code::F20,
        21 => Code::F21,
        22 => Code::F22,
        23 => Code::F23,
        24 => Code::F24,
        _ => Code::Unidentified,
    }
}

fn character_code(character: char) -> Code {
    match character.to_ascii_lowercase() {
        'a' => Code::KeyA,
        'b' => Code::KeyB,
        'c' => Code::KeyC,
        'd' => Code::KeyD,
        'e' => Code::KeyE,
        'f' => Code::KeyF,
        'g' => Code::KeyG,
        'h' => Code::KeyH,
        'i' => Code::KeyI,
        'j' => Code::KeyJ,
        'k' => Code::KeyK,
        'l' => Code::KeyL,
        'm' => Code::KeyM,
        'n' => Code::KeyN,
        'o' => Code::KeyO,
        'p' => Code::KeyP,
        'q' => Code::KeyQ,
        'r' => Code::KeyR,
        's' => Code::KeyS,
        't' => Code::KeyT,
        'u' => Code::KeyU,
        'v' => Code::KeyV,
        'w' => Code::KeyW,
        'x' => Code::KeyX,
        'y' => Code::KeyY,
        'z' => Code::KeyZ,
        '0' => Code::Digit0,
        '1' => Code::Digit1,
        '2' => Code::Digit2,
        '3' => Code::Digit3,
        '4' => Code::Digit4,
        '5' => Code::Digit5,
        '6' => Code::Digit6,
        '7' => Code::Digit7,
        '8' => Code::Digit8,
        '9' => Code::Digit9,
        ' ' => Code::Space,
        '-' => Code::Minus,
        '=' => Code::Equal,
        '[' => Code::BracketLeft,
        ']' => Code::BracketRight,
        '\\' => Code::Backslash,
        ';' => Code::Semicolon,
        '\'' => Code::Quote,
        ',' => Code::Comma,
        '.' => Code::Period,
        '/' => Code::Slash,
        '`' => Code::Backquote,
        _ => Code::Unidentified,
    }
}

fn mouse_button(button: TerminalMouseButton) -> MouseButton {
    match button {
        TerminalMouseButton::Left => MouseButton::Left,
        TerminalMouseButton::Right => MouseButton::Right,
        TerminalMouseButton::Middle => MouseButton::Middle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use static_assertions as sa;

    sa::assert_impl_all!(IdleHandle: Send, Sync);
    sa::assert_not_impl_any!(WindowHandle: Send, Sync);

    #[test]
    fn idle_handle_is_send_and_sync() {
        let queue = Arc::new(Mutex::new(Vec::new()));
        let wake = Arc::new(AtomicBool::new(false));
        let handle = IdleHandle { queue, wake };
        std::thread::spawn(move || handle.add_idle_token(IdleToken::new(1)))
            .join()
            .unwrap();
    }
}
