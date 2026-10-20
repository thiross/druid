// Copyright 2026 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! A small Druid application running on the terminal backend.
//!
//! Run with:
//! `cargo run -p druid --example tui --no-default-features --features tui`

use druid::text::{BigTextSize, RichText};
use druid::widget::{Button, Flex, Label, SelectableLabel, TextBox};
use druid::{theme, AppLauncher, Data, Lens, UnitPoint, WidgetExt, WindowDesc};

#[derive(Clone, Data, Lens)]
struct AppState {
    name: String,
    count: u64,
    note: RichText,
}

fn main() {
    let window = WindowDesc::new(build_root())
        .title("Druid TUI")
        .window_size((640.0, 384.0));

    let state = AppState {
        name: "World".to_string(),
        count: 0,
        note: RichText::new("This read-only text can be selected and copied.".into()),
    };

    AppLauncher::with_window(window)
        .launch(state)
        .expect("failed to launch the TUI application");
}

fn build_root() -> impl druid::Widget<AppState> {
    let title = Label::new("DRUID TUI").with_big_text(BigTextSize::Quadrant);

    let greeting = Label::new(|data: &AppState, _: &druid::Env| {
        if data.name.is_empty() {
            "Hello!".to_string()
        } else {
            format!("Hello {}!", data.name)
        }
    });

    let textbox = TextBox::new()
        .with_placeholder("Enter a name")
        .lens(AppState::name);

    let count =
        Label::new(|data: &AppState, _: &druid::Env| format!("Button presses: {}", data.count));
    let selectable = SelectableLabel::new().lens(AppState::note);

    let button = Button::new("Press me").on_click(|_, data: &mut AppState, _| data.count += 1);

    Flex::column()
        .with_child(title)
        .with_spacer(8.0)
        .with_child(greeting)
        .with_spacer(16.0)
        .with_child(textbox)
        .with_spacer(16.0)
        .with_child(count)
        .with_spacer(16.0)
        .with_child(selectable)
        .with_spacer(16.0)
        .with_child(button)
        .align_vertical(UnitPoint::CENTER)
        .padding(24.0)
        .border(theme::BORDER_LIGHT, 1.0)
}
