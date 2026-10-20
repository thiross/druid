// Copyright 2026 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! A read-only text label with mouse selection and clipboard support.

use std::ops::Range;
use std::sync::Arc;

use tracing::{instrument, trace, warn};

use crate::debug_state::DebugState;
use crate::text::{self, Selection, TextStorage};
use crate::widget::prelude::*;
use crate::widget::{Axis, LineBreaking};
use crate::{
    Application, ArcStr, Code, Color, Cursor, FontDescriptor, HotKey, KeyOrValue, MouseButton,
    Point, Rect, Selector, SysMods, TextAlignment, TextLayout,
};

// Keep this consistent with `RawLabel`.
const LABEL_X_PADDING: f64 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DragGranularity {
    Grapheme,
    Word { start: usize, end: usize },
    Line { start: usize, end: usize },
}

/// A read-only label whose text can be selected with the mouse and copied.
///
/// `SelectableLabel` supports the same text presentation options as
/// [`RawLabel`], plus:
///
/// - click-and-drag selection;
/// - shift-click selection extension;
/// - double-click word selection and triple-click logical-line selection;
/// - `commands::COPY` and `commands::SELECT_ALL`;
/// - `Ctrl+A`, `Ctrl+Y`, `Alt+C`, and `Ctrl+Insert` fallback shortcuts.
///
/// Copying uses the application clipboard and sends a [`SelectableLabel::COPY`]
/// notification containing the copied text. Use [`with_on_copy`] when the
/// application needs to show its own status message.
///
/// # Examples
///
/// ```
/// use druid::text::RichText;
/// use druid::widget::SelectableLabel;
/// use druid::ArcStr;
///
/// let rich_text = RichText::new(ArcStr::from("Read-only, selectable text"));
/// let _: SelectableLabel<RichText> =
///     SelectableLabel::new().with_text_alignment(druid::TextAlignment::Start);
/// ```
///
/// [`RawLabel`]: super::RawLabel
/// [`with_on_copy`]: SelectableLabel::with_on_copy
pub struct SelectableLabel<T> {
    layout: TextLayout<T>,
    line_break_mode: LineBreaking,
    selection: Option<Selection>,
    drag_granularity: DragGranularity,
    disabled: bool,
    default_text_color: KeyOrValue<Color>,
    on_copy: Option<Arc<dyn Fn(&str)>>,
}

impl<T: TextStorage> SelectableLabel<T> {
    /// A notification sent after text is copied.
    ///
    /// The payload is the copied text.
    pub const COPY: Selector<ArcStr> = Selector::new("druid-builtin.selectable-label-copy");

    /// Create a new selectable label.
    ///
    /// Text wraps at word boundaries by default. Use
    /// [`with_line_break_mode`] to opt into clipping or overflow.
    ///
    /// [`with_line_break_mode`]: SelectableLabel::with_line_break_mode
    pub fn new() -> Self {
        Self {
            layout: TextLayout::new(),
            line_break_mode: LineBreaking::WordWrap,
            selection: None,
            drag_granularity: DragGranularity::Grapheme,
            disabled: false,
            default_text_color: crate::theme::TEXT_COLOR.into(),
            on_copy: None,
        }
    }

    /// Builder-style method for setting the text color.
    ///
    /// The argument can be either a `Color` or a [`Key<Color>`].
    ///
    /// [`Key<Color>`]: crate::Key
    pub fn with_text_color(mut self, color: impl Into<KeyOrValue<Color>>) -> Self {
        self.set_text_color(color);
        self
    }

    /// Builder-style method for setting the text size.
    ///
    /// The argument can be either an `f64` or a [`Key<f64>`].
    ///
    /// [`Key<f64>`]: crate::Key
    pub fn with_text_size(mut self, size: impl Into<KeyOrValue<f64>>) -> Self {
        self.set_text_size(size);
        self
    }

    /// Builder-style method for setting the font.
    ///
    /// The argument can be a [`FontDescriptor`] or a
    /// [`Key<FontDescriptor>`].
    ///
    /// [`Key<FontDescriptor>`]: crate::Key
    pub fn with_font(mut self, font: impl Into<KeyOrValue<FontDescriptor>>) -> Self {
        self.set_font(font);
        self
    }

    /// Builder-style method to set the [`LineBreaking`] behavior.
    pub fn with_line_break_mode(mut self, mode: LineBreaking) -> Self {
        self.set_line_break_mode(mode);
        self
    }

    /// Builder-style method to set the [`TextAlignment`].
    pub fn with_text_alignment(mut self, alignment: TextAlignment) -> Self {
        self.set_text_alignment(alignment);
        self
    }

    /// Set a callback invoked after selected text has been copied.
    ///
    /// The callback receives the copied text. It is intended for application
    /// UI such as a status-bar message; the framework does not add any
    /// application-specific copy feedback.
    pub fn with_on_copy(mut self, callback: impl Fn(&str) + 'static) -> Self {
        self.on_copy = Some(Arc::new(callback));
        self
    }

    /// Set the text color.
    pub fn set_text_color(&mut self, color: impl Into<KeyOrValue<Color>>) {
        let color = color.into();
        if !self.disabled {
            self.layout.set_text_color(color.clone());
        }
        self.default_text_color = color;
    }

    /// Set the text size.
    pub fn set_text_size(&mut self, size: impl Into<KeyOrValue<f64>>) {
        self.layout.set_text_size(size);
    }

    /// Set the font.
    pub fn set_font(&mut self, font: impl Into<KeyOrValue<FontDescriptor>>) {
        self.layout.set_font(font);
    }

    /// Set the [`LineBreaking`] behavior.
    pub fn set_line_break_mode(&mut self, mode: LineBreaking) {
        self.line_break_mode = mode;
    }

    /// Set the [`TextAlignment`].
    pub fn set_text_alignment(&mut self, alignment: TextAlignment) {
        self.layout.set_text_alignment(alignment);
    }

    /// Clear the current selection.
    ///
    /// Returns `true` if there was a selection to clear.
    pub fn clear_selection(&mut self) -> bool {
        self.selection.take().is_some()
    }

    /// Return the selected byte range, if any.
    pub fn selected_range(&self) -> Option<Range<usize>> {
        self.selection
            .filter(|selection| !selection.is_caret())
            .map(|selection| selection.range())
    }

    fn text_origin() -> Point {
        Point::new(LABEL_X_PADDING, 0.0)
    }

    fn draw_origin(&self) -> Point {
        self.layout.layout_origin(Self::text_origin())
    }

    fn selection_rects(&self) -> Vec<Rect> {
        let Some(range) = self.selected_range() else {
            return Vec::new();
        };
        let offset = self.draw_origin().to_vec2();
        self.layout
            .rects_for_range(range)
            .into_iter()
            .map(|rect| rect + offset)
            .collect()
    }

    fn position_for_point(&self, point: Point) -> usize {
        self.layout
            .text_position_for_point(point - self.draw_origin().to_vec2())
    }

    fn begin_selection(&mut self, point: Point, count: u8, extend: bool) {
        let position = self.position_for_point(point);
        let Some(text) = self.layout.text() else {
            return;
        };

        if extend {
            let mut selection = self.selection.unwrap_or_else(|| Selection::caret(position));
            selection.active = position;
            self.selection = Some(selection.constrained(text.as_str()));
            self.drag_granularity = DragGranularity::Grapheme;
            return;
        }

        let (range, granularity) = match count {
            2 => {
                let range = text::word_range_at(&self.layout, position);
                (
                    range.clone(),
                    DragGranularity::Word {
                        start: range.start,
                        end: range.end,
                    },
                )
            }
            3 => {
                let range = text::logical_line_range(text.as_str(), position);
                (
                    range.clone(),
                    DragGranularity::Line {
                        start: range.start,
                        end: range.end,
                    },
                )
            }
            _ => (position..position, DragGranularity::Grapheme),
        };

        self.selection = Some(Selection::new(range.start, range.end));
        self.drag_granularity = granularity;
    }

    fn extend_selection(&mut self, point: Point) {
        let Some(selection) = self.selection else {
            return;
        };
        let Some(text) = self.layout.text() else {
            return;
        };
        let position = self.position_for_point(point);
        let updated = match self.drag_granularity {
            DragGranularity::Grapheme => Selection::new(selection.anchor, position),
            DragGranularity::Word { start, end } => {
                let range = text::word_range_at(&self.layout, position);
                if position <= start {
                    Selection::new(end, range.start)
                } else {
                    Selection::new(start, range.end)
                }
            }
            DragGranularity::Line { start, end } => {
                let range = text::logical_line_range(text.as_str(), position);
                if position <= start {
                    Selection::new(end, range.start)
                } else {
                    Selection::new(start, range.end)
                }
            }
        };
        self.selection = Some(updated.constrained(text.as_str()));
    }

    fn select_all(&mut self) -> bool {
        let Some(text) = self.layout.text() else {
            return false;
        };
        if text.as_str().is_empty() {
            return false;
        }
        self.selection = Some(Selection::new(0, text.as_str().len()));
        self.drag_granularity = DragGranularity::Grapheme;
        true
    }

    fn selected_text(&self) -> Option<&str> {
        let range = self.selected_range()?;
        self.layout.text()?.as_str().get(range)
    }

    fn copy_selection(&self, ctx: &mut EventCtx) -> bool {
        let Some(selected) = self.selected_text() else {
            return false;
        };
        if selected.is_empty() {
            return false;
        }

        if let Some(application) = Application::try_global() {
            application.clipboard().put_string(selected);
        }
        if let Some(callback) = &self.on_copy {
            callback(selected);
        }
        ctx.submit_notification(Self::COPY.with(ArcStr::from(selected)));
        true
    }
}

impl<T: TextStorage> Default for SelectableLabel<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: TextStorage> Widget<T> for SelectableLabel<T> {
    #[instrument(
        name = "SelectableLabel",
        level = "trace",
        skip(self, ctx, event, _data, env)
    )]
    fn event(&mut self, ctx: &mut EventCtx, event: &Event, _data: &mut T, env: &Env) {
        match event {
            Event::MouseDown(mouse) if mouse.button == MouseButton::Left && !ctx.is_disabled() => {
                self.layout.rebuild_if_needed(ctx.text(), env);
                ctx.request_focus();
                if !mouse.focus {
                    self.begin_selection(mouse.pos, mouse.count, mouse.mods.shift());
                }
                ctx.set_active(true);
                ctx.request_paint();
                ctx.set_handled();
            }
            Event::MouseMove(mouse) => {
                if ctx.is_disabled() {
                    ctx.clear_cursor();
                    return;
                }

                ctx.set_cursor(&Cursor::IBeam);
                if ctx.is_active() {
                    self.layout.rebuild_if_needed(ctx.text(), env);
                    let previous = self.selection;
                    self.extend_selection(mouse.pos);
                    if previous != self.selection {
                        ctx.request_paint();
                    }
                }
            }
            Event::MouseUp(mouse) if mouse.button == MouseButton::Left && ctx.is_active() => {
                ctx.set_active(false);
                ctx.request_paint();
            }
            Event::KeyDown(key) if !ctx.is_disabled() => {
                let copy = (key.mods.ctrl() && matches!(key.code, Code::KeyY | Code::Insert))
                    || (key.mods.alt() && key.code == Code::KeyC)
                    || HotKey::new(SysMods::Cmd, "c").matches(key);
                let select_all = (key.mods.ctrl() && key.code == Code::KeyA)
                    || HotKey::new(SysMods::Cmd, "a").matches(key);

                if copy && self.copy_selection(ctx) {
                    ctx.set_handled();
                } else if select_all && self.select_all() {
                    ctx.request_paint();
                    ctx.set_handled();
                }
            }
            Event::Command(command)
                if ctx.is_focused() && !ctx.is_disabled() && command.is(crate::commands::COPY) =>
            {
                if self.copy_selection(ctx) {
                    ctx.set_handled();
                }
            }
            Event::Command(command)
                if ctx.is_focused()
                    && !ctx.is_disabled()
                    && command.is(crate::commands::SELECT_ALL)
                    && self.select_all() =>
            {
                ctx.request_paint();
                ctx.set_handled();
            }
            _ => {}
        }
    }

    #[instrument(
        name = "SelectableLabel",
        level = "trace",
        skip(self, ctx, event, data, _env)
    )]
    fn lifecycle(&mut self, ctx: &mut LifeCycleCtx, event: &LifeCycle, data: &T, _env: &Env) {
        match event {
            LifeCycle::WidgetAdded => {
                self.layout.set_text(data.clone());
            }
            LifeCycle::BuildFocusChain => {
                ctx.register_for_focus();
            }
            LifeCycle::DisabledChanged(disabled) => {
                self.disabled = *disabled;
                let color = if *disabled {
                    KeyOrValue::Key(crate::theme::DISABLED_TEXT_COLOR)
                } else {
                    self.default_text_color.clone()
                };
                self.layout.set_text_color(color);
                ctx.request_layout();
            }
            LifeCycle::FocusChanged(_) => {
                ctx.request_paint();
            }
            _ => {}
        }
    }

    #[instrument(
        name = "SelectableLabel",
        level = "trace",
        skip(self, ctx, old_data, data, _env)
    )]
    fn update(&mut self, ctx: &mut UpdateCtx, old_data: &T, data: &T, _env: &Env) {
        if !old_data.same(data) {
            self.layout.set_text(data.clone());
            self.selection = None;
            self.drag_granularity = DragGranularity::Grapheme;
            ctx.request_layout();
        } else if let Some(selection) = self.selection {
            self.selection = Some(selection.constrained(data.as_str()));
        }
        if self.layout.needs_rebuild_after_update(ctx) {
            ctx.request_layout();
        }
    }

    #[instrument(
        name = "SelectableLabel",
        level = "trace",
        skip(self, ctx, bc, _data, env)
    )]
    fn layout(&mut self, ctx: &mut LayoutCtx, bc: &BoxConstraints, _data: &T, env: &Env) -> Size {
        bc.debug_check("SelectableLabel");

        let width = match self.line_break_mode {
            LineBreaking::WordWrap => (bc.max().width - LABEL_X_PADDING * 2.0).max(0.0),
            _ => f64::INFINITY,
        };

        self.layout.set_wrap_width(width);
        self.layout.rebuild_if_needed(ctx.text(), env);

        let metrics = self.layout.layout_metrics();
        ctx.set_baseline_offset(metrics.size.height - metrics.first_baseline);
        let size = bc.constrain(Size::new(
            metrics.size.width + 2.0 * LABEL_X_PADDING,
            metrics.size.height,
        ));
        trace!("Computed size: {}", size);
        size
    }

    #[instrument(name = "SelectableLabel", level = "trace", skip(self, ctx, _data, env))]
    fn paint(&mut self, ctx: &mut PaintCtx, _data: &T, env: &Env) {
        let label_size = ctx.size();
        if self.line_break_mode == LineBreaking::Clip {
            ctx.clip(label_size.to_rect());
        }

        let selection_color = if ctx.is_focused() {
            env.get(crate::theme::SELECTED_TEXT_BACKGROUND_COLOR)
        } else {
            env.get(crate::theme::SELECTED_TEXT_INACTIVE_BACKGROUND_COLOR)
        };
        for rect in self.selection_rects() {
            ctx.fill(rect, &selection_color);
        }
        self.layout.draw(ctx, Self::text_origin());
    }

    fn debug_state(&self, _data: &T) -> DebugState {
        DebugState {
            display_name: self.short_type_name().to_string(),
            main_value: self
                .layout
                .text()
                .map(|text| text.as_str().to_owned())
                .unwrap_or_default(),
            ..Default::default()
        }
    }

    fn compute_max_intrinsic(
        &mut self,
        axis: Axis,
        ctx: &mut LayoutCtx,
        bc: &BoxConstraints,
        data: &T,
        env: &Env,
    ) -> f64 {
        match axis {
            Axis::Horizontal => match self.line_break_mode {
                LineBreaking::WordWrap => {
                    self.line_break_mode = LineBreaking::Clip;
                    let size = self.layout(ctx, bc, data, env);
                    self.line_break_mode = LineBreaking::WordWrap;
                    size.width
                }
                _ => self.layout(ctx, bc, data, env).width,
            },
            Axis::Vertical => {
                warn!("Max intrinsic height of a selectable label is not implemented.");
                0.0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::tests::{harness::Harness, move_mouse};
    #[cfg(feature = "tui")]
    use crate::widget::Padding;
    #[cfg(feature = "tui")]
    use crate::Insets;
    use crate::{KbKey, KeyEvent, Modifiers, MouseButtons, MouseEvent, WidgetExt, WidgetId};

    fn mouse_event(pos: Point, button: MouseButton, count: u8) -> MouseEvent {
        let mut event = move_mouse(pos);
        event.button = button;
        event.count = count;
        event.buttons = match button {
            MouseButton::Left => MouseButtons::new().with(MouseButton::Left),
            _ => MouseButtons::new(),
        };
        event
    }

    fn select_and_copy(
        text: &str,
        down: Point,
        drag_to: Option<Point>,
        count: u8,
    ) -> Option<String> {
        let copied = Rc::new(RefCell::new(None));
        let copied_for_callback = copied.clone();
        let id = WidgetId::next();
        let label = SelectableLabel::<String>::new()
            .with_on_copy(move |text| {
                *copied_for_callback.borrow_mut() = Some(text.to_owned());
            })
            .with_id(id);

        Harness::create_simple(text.to_owned(), label, |harness| {
            harness.send_initial_events();
            harness.just_layout();
            harness.event(Event::MouseDown(mouse_event(
                down,
                MouseButton::Left,
                count,
            )));
            if let Some(point) = drag_to {
                harness.event(Event::MouseMove(move_mouse(point)));
            }
            harness.event(Event::MouseUp(mouse_event(
                drag_to.unwrap_or(down),
                MouseButton::Left,
                0,
            )));
            harness.submit_command(crate::commands::COPY.to(id));
        });

        let copied_text = copied.borrow().clone();
        copied_text
    }

    #[test]
    fn double_click_selects_word_and_copy_notifies() {
        assert_eq!(
            select_and_copy("hello world", Point::new(8.0, 8.0), None, 2).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn triple_click_selects_logical_line() {
        assert_eq!(
            select_and_copy("first\nsecond\nthird", Point::new(8.0, 8.0), None, 3).as_deref(),
            Some("first")
        );
    }

    #[test]
    fn drag_selects_text() {
        let copied = select_and_copy(
            "hello world",
            Point::new(8.0, 8.0),
            Some(Point::new(48.0, 8.0)),
            1,
        );
        assert!(copied.is_some_and(|text| !text.is_empty()));
    }

    #[test]
    fn shift_click_extends_selection() {
        let copied = Rc::new(RefCell::new(None));
        let copied_for_callback = copied.clone();
        let id = WidgetId::next();
        let label = SelectableLabel::<String>::new()
            .with_on_copy(move |text| {
                *copied_for_callback.borrow_mut() = Some(text.to_owned());
            })
            .with_id(id);

        Harness::create_simple("hello".to_owned(), label, |harness| {
            harness.send_initial_events();
            harness.just_layout();
            harness.event(Event::MouseDown(mouse_event(
                Point::new(8.0, 8.0),
                MouseButton::Left,
                1,
            )));
            harness.event(Event::MouseUp(mouse_event(
                Point::new(8.0, 8.0),
                MouseButton::Left,
                0,
            )));

            let mut extend = mouse_event(Point::new(64.0, 8.0), MouseButton::Left, 1);
            extend.mods = Modifiers::SHIFT;
            harness.event(Event::MouseDown(extend));
            harness.event(Event::MouseUp(mouse_event(
                Point::new(64.0, 8.0),
                MouseButton::Left,
                0,
            )));
            harness.submit_command(crate::commands::COPY.to(id));
        });

        assert_eq!(copied.borrow().as_deref(), Some("ello"));
    }

    #[test]
    fn select_all_command_selects_entire_text() {
        let copied = Rc::new(RefCell::new(None));
        let copied_for_callback = copied.clone();
        let id = WidgetId::next();
        let label = SelectableLabel::<String>::new()
            .with_on_copy(move |text| {
                *copied_for_callback.borrow_mut() = Some(text.to_owned());
            })
            .with_id(id);

        Harness::create_simple("hello\nworld".to_owned(), label, |harness| {
            harness.send_initial_events();
            harness.just_layout();
            harness.event(Event::MouseDown(mouse_event(
                Point::new(8.0, 8.0),
                MouseButton::Left,
                1,
            )));
            harness.event(Event::MouseUp(mouse_event(
                Point::new(8.0, 8.0),
                MouseButton::Left,
                0,
            )));
            harness.submit_command(crate::commands::SELECT_ALL.to(id));
            harness.submit_command(crate::commands::COPY.to(id));
        });

        assert_eq!(copied.borrow().as_deref(), Some("hello\nworld"));
    }

    #[test]
    fn empty_selection_does_not_copy() {
        assert_eq!(
            select_and_copy("hello", Point::new(8.0, 8.0), None, 1),
            None
        );
    }

    #[test]
    fn cjk_double_click_selects_word() {
        assert_eq!(
            select_and_copy("你好 世界", Point::new(8.0, 8.0), None, 2).as_deref(),
            Some("你")
        );
    }

    #[test]
    fn copy_shortcuts_use_expected_keys() {
        let copied = Rc::new(RefCell::new(None));
        let copied_for_callback = copied.clone();
        let id = WidgetId::next();
        let label = SelectableLabel::<String>::new()
            .with_on_copy(move |text| {
                *copied_for_callback.borrow_mut() = Some(text.to_owned());
            })
            .with_id(id);

        Harness::create_simple("hello".to_owned(), label, |harness| {
            harness.send_initial_events();
            harness.just_layout();
            harness.event(Event::MouseDown(mouse_event(
                Point::new(8.0, 8.0),
                MouseButton::Left,
                2,
            )));
            harness.event(Event::MouseUp(mouse_event(
                Point::new(8.0, 8.0),
                MouseButton::Left,
                0,
            )));
            let mut key = KeyEvent::for_test(Modifiers::CONTROL, KbKey::Character("y".into()));
            key.code = Code::KeyY;
            harness.event(Event::KeyDown(key));
        });

        assert_eq!(copied.borrow().as_deref(), Some("hello"));
    }

    #[test]
    #[cfg(feature = "tui")]
    fn half_cell_origin_click_reaches_drawn_text() {
        let copied = Rc::new(RefCell::new(None));
        let copied_for_callback = copied.clone();
        let id = WidgetId::next();
        let label = SelectableLabel::<String>::new()
            .with_on_copy(move |text| {
                *copied_for_callback.borrow_mut() = Some(text.to_owned());
            })
            .with_id(id);
        let nested = Padding::new(Insets::new(0.0, 4.0, 0.0, 0.0), label);
        let root = Padding::new(Insets::new(0.0, 4.0, 0.0, 0.0), nested);

        Harness::create_simple("hello".to_owned(), root, |harness| {
            harness.send_initial_events();
            harness.just_layout();

            // Each padding level contributes four pixels. The outer container
            // paints at y=8, then the inner one paints at y=16, where the
            // visible first text line starts.
            harness.event(Event::MouseDown(mouse_event(
                Point::new(8.0, 24.0),
                MouseButton::Left,
                2,
            )));
            harness.event(Event::MouseUp(mouse_event(
                Point::new(8.0, 24.0),
                MouseButton::Left,
                0,
            )));
            harness.submit_command(crate::commands::COPY.to(id));
        });

        assert_eq!(copied.borrow().as_deref(), Some("hello"));
    }
}
