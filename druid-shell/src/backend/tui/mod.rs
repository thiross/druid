// Copyright 2026 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! Terminal backend support, rendered through `piet-tui` and ratatui.

pub mod application;
pub mod clipboard;
pub mod error;
pub mod menu;
pub mod screen;
pub mod window;

use crate::kurbo::Size;
use crate::piet::{ratatui_core, PietText, RenderContext, TuiConfig, TuiRenderContext};

pub(crate) const CELL_WIDTH: f64 = 8.0;
pub(crate) const CELL_HEIGHT: f64 = 16.0;

pub(crate) fn cell_size() -> Size {
    Size::new(CELL_WIDTH, CELL_HEIGHT)
}

pub(crate) fn config() -> TuiConfig {
    TuiConfig::with_cell_size(cell_size())
}

pub(crate) fn logical_size(cols: u16, rows: u16) -> Size {
    Size::new(f64::from(cols) * CELL_WIDTH, f64::from(rows) * CELL_HEIGHT)
}

pub(crate) fn text_factory() -> PietText {
    let mut buffer =
        ratatui_core::buffer::Buffer::empty(ratatui_core::layout::Rect::new(0, 0, 1, 1));
    TuiRenderContext::new(&mut buffer, config()).text().clone()
}
