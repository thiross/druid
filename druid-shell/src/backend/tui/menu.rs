// Copyright 2026 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! Terminal menus.
//!
//! Menus are not exposed by the terminal backend yet. The API is kept so that
//! applications can build platform-independent window descriptions.

use crate::hotkey::HotKey;

#[derive(Debug, Default)]
pub struct Menu;

impl Menu {
    pub fn new() -> Menu {
        Menu
    }

    pub fn new_for_popup() -> Menu {
        Menu
    }

    pub fn add_dropdown(&mut self, _menu: Menu, _text: &str, _enabled: bool) {}

    pub fn add_item(
        &mut self,
        _id: u32,
        _text: &str,
        _key: Option<&HotKey>,
        _selected: Option<bool>,
        _enabled: bool,
    ) {
    }

    pub fn add_separator(&mut self) {}
}
