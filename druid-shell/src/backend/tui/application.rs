// Copyright 2026 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! Terminal implementation of application-level features.

use std::cell::RefCell;
use std::io::{self, stdout};
use std::rc::Rc;
use std::time::Duration;

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui::crossterm::execute;
use ratatui::DefaultTerminal;

use super::clipboard::Clipboard;
use super::error::Error;
use super::window::WindowState;
use crate::application::AppHandler;

const MAX_POLL_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Default)]
pub(crate) struct AppState {
    pub(crate) windows: Vec<Rc<WindowState>>,
    pub(crate) quit: bool,
}

#[derive(Clone)]
pub(crate) struct Application {
    pub(crate) state: Rc<RefCell<AppState>>,
}

impl Application {
    pub fn new() -> Result<Application, Error> {
        Ok(Application {
            state: Rc::new(RefCell::new(AppState::default())),
        })
    }

    pub fn run(self, _handler: Option<Box<dyn AppHandler>>) {
        if self.state.borrow().windows.is_empty() {
            return;
        }

        let mut terminal = match ratatui::try_init() {
            Ok(terminal) => terminal,
            Err(error) => {
                tracing::error!("failed to initialize terminal: {error}");
                return;
            }
        };

        let _ = execute!(stdout(), EnableMouseCapture, EnableBracketedPaste);
        if let Ok(area) = terminal.size() {
            for window in self.state.borrow().windows.clone() {
                window.set_terminal_size(area.width, area.height);
            }
        }
        if let Some(window) = self.state.borrow().windows.first() {
            window.focus();
        }

        if let Err(error) = run_loop(&self.state, &mut terminal) {
            tracing::error!("terminal run loop stopped: {error}");
        }

        let _ = execute!(stdout(), DisableBracketedPaste, DisableMouseCapture);
        if let Err(error) = ratatui::try_restore() {
            tracing::error!("failed to restore terminal: {error}");
        }

        for window in self.state.borrow().windows.clone() {
            window.destroy();
        }
        self.state.borrow_mut().windows.clear();
    }

    pub fn quit(&self) {
        self.state.borrow_mut().quit = true;
    }

    pub fn clipboard(&self) -> Clipboard {
        Clipboard
    }

    pub fn get_locale() -> String {
        let locale = std::env::var("LC_ALL")
            .or_else(|_| std::env::var("LC_MESSAGES"))
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_else(|_| "en-US".to_string());
        let locale = locale
            .split(['.', '@'])
            .next()
            .unwrap_or("en-US")
            .replace('_', "-");
        if locale.is_empty() {
            "en-US".to_string()
        } else {
            locale
        }
    }
}

fn run_loop(state: &Rc<RefCell<AppState>>, terminal: &mut DefaultTerminal) -> io::Result<()> {
    loop {
        if state.borrow().quit {
            break;
        }

        let windows = state.borrow().windows.clone();
        if windows.is_empty() {
            break;
        }

        for window in &windows {
            window.process_timers();
            window.process_idle();
        }

        let windows = state.borrow().windows.clone();
        for window in &windows {
            if window.should_draw() {
                terminal.draw(|frame| window.paint(frame))?;
            }
        }

        cleanup_closed_windows(state);
        if state.borrow().quit || state.borrow().windows.is_empty() {
            break;
        }

        let windows = state.borrow().windows.clone();
        let has_pending_work = windows.iter().any(|window| window.has_pending_work());
        let timeout = if has_pending_work {
            Duration::ZERO
        } else {
            MAX_POLL_INTERVAL
        };

        if event::poll(timeout)? {
            let event = event::read()?;
            for window in &windows {
                window.handle_event(event.clone());
            }
        }

        cleanup_closed_windows(state);
    }

    Ok(())
}

fn cleanup_closed_windows(state: &Rc<RefCell<AppState>>) {
    let closed: Vec<_> = state
        .borrow()
        .windows
        .iter()
        .filter(|window| window.is_closed())
        .cloned()
        .collect();
    for window in closed {
        window.destroy();
    }
    state
        .borrow_mut()
        .windows
        .retain(|window| !window.is_closed());
}

pub fn init_harness() {}

#[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "openbsd"))]
impl crate::platform::linux::ApplicationExt for crate::Application {
    fn primary_clipboard(&self) -> crate::Clipboard {
        crate::Clipboard(Clipboard)
    }
}
