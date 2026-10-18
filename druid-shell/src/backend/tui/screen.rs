// Copyright 2026 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! Terminal screen information.

use ratatui::crossterm::terminal;

use super::{CELL_HEIGHT, CELL_WIDTH};
use crate::kurbo::{Point, Rect, Size};
use crate::screen::Monitor;

pub(crate) fn get_monitors() -> Vec<Monitor> {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let pixel = Size::new(f64::from(cols) * CELL_WIDTH, f64::from(rows) * CELL_HEIGHT);
    let rect = Rect::from_origin_size(Point::ZERO, pixel);
    vec![Monitor::new(true, rect, rect)]
}
