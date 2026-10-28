// Copyright 2026 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! A read-only text label with mouse selection and clipboard support.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;

use tracing::{instrument, trace, warn};
use unicode_segmentation::UnicodeSegmentation;

use crate::debug_state::DebugState;
use crate::piet::TextLayout as _;
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

/// The phase of a [`SelectableLabel`] selection change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionChangeKind {
    /// A new selection gesture started.
    Started,
    /// The active selection changed.
    Updated,
    /// The selection gesture finished.
    Finished,
    /// The selection was cleared.
    Cleared,
}

/// A notification emitted when a [`SelectableLabel`] selection changes.
///
/// The [`Notification::source`] identifies the label that emitted the change.
///
/// [`Notification::source`]: crate::Notification::source
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectionChange {
    /// The phase of the change.
    pub kind: SelectionChangeKind,
    /// The selection after the change, including a caret selection.
    pub selection: Option<Selection>,
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
    pending_selection_changes: VecDeque<SelectionChange>,
    paint_on_next_event: bool,
}

impl<T: TextStorage> SelectableLabel<T> {
    /// A notification sent after text is copied.
    ///
    /// The payload is the copied text.
    pub const COPY: Selector<ArcStr> = Selector::new("druid-builtin.selectable-label-copy");

    /// A notification emitted when the selection changes.
    ///
    /// Use [`Notification::source`] to identify which label changed. The
    /// notification is delivered through the label's ancestors, so a
    /// controller wrapping a label can coordinate a selection across
    /// multiple labels.
    ///
    /// [`Notification::source`]: crate::Notification::source
    pub const SELECTION_CHANGED: Selector<SelectionChange> =
        Selector::new("druid-builtin.selectable-label-selection-changed");

    /// A command that replaces this label's selection.
    ///
    /// Target the command to a specific label. This is the coordination
    /// primitive for containers or sibling controllers that need to assign
    /// selection ranges without access to the label's concrete widget type.
    pub const SET_SELECTION: Selector<Option<Selection>> =
        Selector::new("druid-builtin.selectable-label-set-selection");

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
            pending_selection_changes: VecDeque::new(),
            paint_on_next_event: false,
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
        let had_selection = self.selection.is_some();
        self.set_selection(None);
        had_selection
    }

    /// Return the current selection, including a caret selection.
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// Replace the current selection.
    ///
    /// Byte indices are clamped to the current text and snapped to the next
    /// grapheme and text-layout boundary. The direction of the selection
    /// (`anchor..active`) is preserved.
    ///
    /// This method does not require synthetic pointer or keyboard events.
    /// Selection-change notifications and paint requests are queued until this
    /// widget handles its next event. A controller can call this method before
    /// forwarding an event to the child so both happen during that event.
    pub fn set_selection(&mut self, selection: Option<Selection>) {
        let previous = self.selection;
        let selection = selection.map(|selection| self.constrain_selection(selection));
        if previous == selection {
            return;
        }

        let previous_range = Self::visible_range(previous);
        let new_range = Self::visible_range(selection);
        self.selection = selection;
        self.drag_granularity = DragGranularity::Grapheme;
        if previous_range != new_range {
            self.paint_on_next_event = true;
        }

        let kind = if selection.is_none() {
            SelectionChangeKind::Cleared
        } else if previous.is_none() {
            SelectionChangeKind::Started
        } else {
            SelectionChangeKind::Updated
        };
        self.queue_selection_change(kind);
    }

    /// Return the selected byte range, if any.
    pub fn selected_range(&self) -> Option<Range<usize>> {
        self.selection
            .filter(|selection| !selection.is_caret())
            .map(|selection| selection.range())
    }

    /// Return an owned copy of the selected text.
    ///
    /// This returns only the text selected in this label. Composing selected
    /// fragments across multiple labels is the application's responsibility.
    pub fn selected_text(&self) -> Option<ArcStr> {
        let range = self.selected_range()?;
        self.layout.text()?.as_str().get(range).map(ArcStr::from)
    }

    /// Map a point in this label's local coordinate space to a text byte index.
    ///
    /// The result is always a grapheme and text-layout boundary. Returns
    /// `None` if the label has not been laid out yet.
    pub fn text_position_for_point(&self, point: Point) -> Option<usize> {
        let layout = self.layout.layout()?;
        let text = self.layout.text()?.as_str();
        let position = layout
            .hit_test_point(point - self.draw_origin().to_vec2())
            .idx;
        Some(Self::next_grapheme_boundary(text, position))
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
        self.text_position_for_point(point).unwrap_or_default()
    }

    fn visible_range(selection: Option<Selection>) -> Option<Range<usize>> {
        selection
            .filter(|selection| !selection.is_caret())
            .map(|selection| selection.range())
    }

    fn next_grapheme_boundary(text: &str, position: usize) -> usize {
        let position = position.min(text.len());
        if position == 0 || position == text.len() {
            return position;
        }

        for (boundary, _) in text.grapheme_indices(true) {
            if boundary >= position {
                return boundary;
            }
        }
        text.len()
    }

    fn constrain_selection(&self, mut selection: Selection) -> Selection {
        let Some(text) = self.layout.text() else {
            selection.anchor = 0;
            selection.active = 0;
            return selection;
        };
        let text = text.as_str();
        selection.anchor = Self::next_grapheme_boundary(text, selection.anchor);
        selection.active = Self::next_grapheme_boundary(text, selection.active);

        if let Some(layout) = self.layout.layout() {
            let anchor = layout.hit_test_text_position(selection.anchor).point;
            let active = layout.hit_test_text_position(selection.active).point;
            selection.anchor = layout.hit_test_point(anchor).idx;
            selection.active = layout.hit_test_point(active).idx;
            selection.anchor = Self::next_grapheme_boundary(text, selection.anchor);
            selection.active = Self::next_grapheme_boundary(text, selection.active);
        }
        selection
    }

    fn queue_selection_change(&mut self, kind: SelectionChangeKind) {
        self.pending_selection_changes.push_back(SelectionChange {
            kind,
            selection: self.selection,
        });
    }

    fn flush_selection_changes(&mut self, ctx: &mut EventCtx) {
        if self.paint_on_next_event {
            ctx.request_paint();
            self.paint_on_next_event = false;
        }
        for change in self.pending_selection_changes.drain(..) {
            ctx.submit_notification(Self::SELECTION_CHANGED.with(change));
        }
    }

    fn replace_selection(
        &mut self,
        selection: Option<Selection>,
        kind: SelectionChangeKind,
        reset_drag_granularity: bool,
        always_notify: bool,
    ) -> bool {
        let previous = self.selection;
        if previous == selection {
            if always_notify {
                self.queue_selection_change(kind);
            }
            return false;
        }

        if Self::visible_range(previous) != Self::visible_range(selection) {
            self.paint_on_next_event = true;
        }
        self.selection = selection;
        if reset_drag_granularity {
            self.drag_granularity = DragGranularity::Grapheme;
        }
        self.queue_selection_change(kind);
        true
    }

    fn begin_selection(&mut self, point: Point, count: u8, extend: bool) {
        let position = self.position_for_point(point);
        let Some(text) = self.layout.text() else {
            return;
        };

        if extend {
            let mut selection = self.selection.unwrap_or_else(|| Selection::caret(position));
            selection.active = position;
            let selection = self.constrain_selection(selection);
            self.drag_granularity = DragGranularity::Grapheme;
            self.replace_selection(Some(selection), SelectionChangeKind::Updated, false, true);
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

        let selection = self.constrain_selection(Selection::new(range.start, range.end));
        self.replace_selection(Some(selection), SelectionChangeKind::Started, false, true);
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
        let updated = self.constrain_selection(updated);
        self.replace_selection(Some(updated), SelectionChangeKind::Updated, false, false);
    }

    fn select_all(&mut self) -> bool {
        let Some(text) = self.layout.text() else {
            return false;
        };
        if text.as_str().is_empty() {
            return false;
        }
        self.replace_selection(
            Some(Selection::new(0, text.as_str().len())),
            SelectionChangeKind::Updated,
            true,
            false,
        );
        true
    }

    fn copy_selection(&self, ctx: &mut EventCtx) -> bool {
        let Some(selected) = self.selected_text() else {
            return false;
        };
        let selected = selected.as_ref();
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
        self.flush_selection_changes(ctx);
        match event {
            Event::Command(command) if command.is(Self::SET_SELECTION) => {
                self.set_selection(*command.get_unchecked(Self::SET_SELECTION));
                self.flush_selection_changes(ctx);
                ctx.set_handled();
            }
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
                self.queue_selection_change(SelectionChangeKind::Finished);
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
        self.flush_selection_changes(ctx);
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
                if let Some(selection) = self.selection {
                    let constrained = self.constrain_selection(selection);
                    if constrained != selection {
                        self.selection = Some(constrained);
                        self.queue_selection_change(SelectionChangeKind::Updated);
                    }
                }
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
        if self.paint_on_next_event {
            ctx.request_paint();
            self.paint_on_next_event = false;
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
            if let Some(selection) = self.selection {
                self.selection = Some(self.constrain_selection(selection));
                self.paint_on_next_event = true;
                self.queue_selection_change(SelectionChangeKind::Updated);
            }
            self.drag_granularity = DragGranularity::Grapheme;
            ctx.request_layout();
        } else if let Some(selection) = self.selection {
            let constrained = self.constrain_selection(selection);
            if constrained != selection {
                self.selection = Some(constrained);
                self.paint_on_next_event = true;
                self.queue_selection_change(SelectionChangeKind::Updated);
            }
        }
        if self.layout.needs_rebuild_after_update(ctx) {
            ctx.request_layout();
        }
        if self.paint_on_next_event {
            ctx.request_paint();
            self.paint_on_next_event = false;
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
    use crate::tests::helpers::{Record, Recording, TestWidgetExt};
    use crate::tests::{harness::Harness, move_mouse};
    use crate::widget::{Controller, Padding, SizedBox};
    use crate::{
        Event, Insets, KbKey, KeyEvent, Modifiers, MouseButtons, MouseEvent, WidgetExt, WidgetId,
    };

    const PROBE: Selector<Probe> = Selector::new("druid-tests.selectable-label-probe");

    #[derive(Clone, Copy, Debug)]
    enum Probe {
        Set(Option<Selection>),
        HitTestAt(usize),
    }

    #[derive(Clone, Debug, Default)]
    struct Observation {
        selection: Option<Selection>,
        selected_text: Option<ArcStr>,
        position: Option<usize>,
    }

    #[derive(Clone)]
    struct ProbeController {
        observation: Rc<RefCell<Observation>>,
    }

    impl ProbeController {
        fn new(observation: Rc<RefCell<Observation>>) -> Self {
            Self { observation }
        }

        fn observe(&self, child: &SelectableLabel<String>) {
            *self.observation.borrow_mut() = Observation {
                selection: child.selection(),
                selected_text: child.selected_text(),
                position: None,
            };
        }
    }

    impl Controller<String, SelectableLabel<String>> for ProbeController {
        fn event(
            &mut self,
            child: &mut SelectableLabel<String>,
            ctx: &mut EventCtx,
            event: &Event,
            data: &mut String,
            env: &Env,
        ) {
            if let Event::Command(command) = event {
                if let Some(probe) = command.get(PROBE) {
                    match *probe {
                        Probe::Set(selection) => {
                            child.set_selection(selection);
                            // Forwarding the command flushes the setter's paint request and
                            // notification without relying on synthetic input events.
                            child.event(ctx, event, data, env);
                            self.observe(child);
                        }
                        Probe::HitTestAt(position) => {
                            let point = child.layout.point_for_text_position(position)
                                + child.draw_origin().to_vec2();
                            let mut observation = self.observation.borrow_mut();
                            observation.position = child.text_position_for_point(point);
                            observation.selection = child.selection();
                            observation.selected_text = child.selected_text();
                        }
                    }
                    ctx.set_handled();
                    return;
                }
            }

            child.event(ctx, event, data, env);
            self.observe(child);
        }

        fn update(
            &mut self,
            child: &mut SelectableLabel<String>,
            ctx: &mut UpdateCtx,
            old_data: &String,
            data: &String,
            env: &Env,
        ) {
            child.update(ctx, old_data, data, env);
            self.observe(child);
        }
    }

    fn selection_changes(recording: &Recording) -> Vec<(WidgetId, SelectionChange)> {
        recording
            .drain()
            .filter_map(|record| match record {
                Record::E(Event::Notification(notification))
                    if notification.is(SelectableLabel::<String>::SELECTION_CHANGED) =>
                {
                    Some((
                        notification.source(),
                        notification
                            .get(SelectableLabel::<String>::SELECTION_CHANGED)
                            .copied()
                            .unwrap(),
                    ))
                }
                _ => None,
            })
            .collect()
    }

    fn assert_nothing_painted(harness: &mut Harness<String>) {
        harness.paint();
        assert!(harness.window().invalid().is_empty());
    }

    fn record_label(
        label: impl Widget<String> + 'static,
        recording: &Recording,
    ) -> impl Widget<String> {
        // A parent container gives the label its own WidgetPod, so its
        // notifications can bubble through the Recording widget.
        Padding::new(Insets::ZERO, label).record(recording)
    }

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
    fn programmatic_selection_sets_text_and_requests_paint() {
        let observation = Rc::new(RefCell::new(Observation::default()));
        let observation_for_controller = observation.clone();
        let recording = Recording::default();
        let id = WidgetId::next();
        let label = SelectableLabel::<String>::new()
            .controller(ProbeController::new(observation_for_controller))
            .with_id(id);
        let root = record_label(label, &recording);

        Harness::create_simple("hello world".to_owned(), root, |harness| {
            harness.send_initial_events();
            harness.just_layout();
            assert_nothing_painted(harness);
            recording.clear();

            harness.submit_command(PROBE.with(Probe::Set(Some(Selection::new(1, 6)))).to(id));

            let observed = observation.borrow();
            assert_eq!(observed.selection, Some(Selection::new(1, 6)));
            assert_eq!(observed.selected_text.as_deref(), Some("ello "));
            drop(observed);
            assert!(!harness.window().invalid().is_empty());

            let changes = selection_changes(&recording);
            assert_eq!(
                changes,
                vec![(
                    id,
                    SelectionChange {
                        kind: SelectionChangeKind::Started,
                        selection: Some(Selection::new(1, 6)),
                    }
                )]
            );
        });
    }

    #[test]
    fn programmatic_clear_removes_selection_and_notifies() {
        let observation = Rc::new(RefCell::new(Observation::default()));
        let observation_for_controller = observation.clone();
        let recording = Recording::default();
        let id = WidgetId::next();
        let label = SelectableLabel::<String>::new()
            .controller(ProbeController::new(observation_for_controller))
            .with_id(id);
        let root = record_label(label, &recording);

        Harness::create_simple("hello".to_owned(), root, |harness| {
            harness.send_initial_events();
            harness.just_layout();
            harness.submit_command(PROBE.with(Probe::Set(Some(Selection::new(1, 4)))).to(id));
            assert_nothing_painted(harness);
            recording.clear();

            harness.submit_command(PROBE.with(Probe::Set(None)).to(id));

            assert_eq!(observation.borrow().selection, None);
            assert_eq!(observation.borrow().selected_text, None);
            assert!(!harness.window().invalid().is_empty());
            assert_eq!(
                selection_changes(&recording),
                vec![(
                    id,
                    SelectionChange {
                        kind: SelectionChangeKind::Cleared,
                        selection: None,
                    }
                )]
            );
        });
    }

    #[test]
    fn external_selection_snaps_to_utf8_grapheme_boundaries() {
        let observation = Rc::new(RefCell::new(Observation::default()));
        let observation_for_controller = observation.clone();
        let recording = Recording::default();
        let id = WidgetId::next();
        let label = SelectableLabel::<String>::new()
            .controller(ProbeController::new(observation_for_controller))
            .with_id(id);
        let root = record_label(label, &recording);

        Harness::create_simple("éx".to_owned(), root, |harness| {
            harness.send_initial_events();
            harness.just_layout();

            harness.submit_command(
                PROBE
                    .with(Probe::Set(Some(Selection::new(1, usize::MAX))))
                    .to(id),
            );
            assert_eq!(observation.borrow().selection, Some(Selection::new(2, 3)));
            assert_eq!(observation.borrow().selected_text.as_deref(), Some("x"));
        });

        let observation = Rc::new(RefCell::new(Observation::default()));
        let observation_for_controller = observation.clone();
        let recording = Recording::default();
        let id = WidgetId::next();
        let label = SelectableLabel::<String>::new()
            .controller(ProbeController::new(observation_for_controller))
            .with_id(id);
        let root = record_label(label, &recording);

        Harness::create_simple("e\u{301}x".to_owned(), root, |harness| {
            harness.send_initial_events();
            harness.just_layout();

            harness.submit_command(PROBE.with(Probe::Set(Some(Selection::new(1, 3)))).to(id));
            assert_eq!(observation.borrow().selection, Some(Selection::new(3, 3)));
        });
    }

    #[test]
    fn mouse_gesture_and_shift_click_emit_selection_phases() {
        let observation = Rc::new(RefCell::new(Observation::default()));
        let observation_for_controller = observation.clone();
        let recording = Recording::default();
        let id = WidgetId::next();
        let label = SelectableLabel::<String>::new()
            .controller(ProbeController::new(observation_for_controller))
            .with_id(id);
        let root = record_label(label, &recording);

        Harness::create_simple("hello world".to_owned(), root, |harness| {
            harness.send_initial_events();
            harness.just_layout();
            recording.clear();

            harness.event(Event::MouseDown(mouse_event(
                Point::new(8.0, 8.0),
                MouseButton::Left,
                1,
            )));
            harness.event(Event::MouseMove(move_mouse(Point::new(64.0, 8.0))));
            harness.event(Event::MouseUp(mouse_event(
                Point::new(64.0, 8.0),
                MouseButton::Left,
                0,
            )));
            let kinds: Vec<_> = selection_changes(&recording)
                .into_iter()
                .map(|(source, change)| {
                    assert_eq!(source, id);
                    change.kind
                })
                .collect();
            assert_eq!(
                kinds,
                vec![
                    SelectionChangeKind::Started,
                    SelectionChangeKind::Updated,
                    SelectionChangeKind::Finished
                ]
            );

            let anchor = observation.borrow().selection.unwrap().anchor;
            let mut extend = mouse_event(Point::new(96.0, 8.0), MouseButton::Left, 1);
            extend.mods = Modifiers::SHIFT;
            harness.event(Event::MouseDown(extend));
            harness.event(Event::MouseUp(mouse_event(
                Point::new(96.0, 8.0),
                MouseButton::Left,
                0,
            )));
            let changes = selection_changes(&recording);
            assert_eq!(changes[0].1.kind, SelectionChangeKind::Updated);
            assert_eq!(changes[1].1.kind, SelectionChangeKind::Finished);
            assert_eq!(changes[0].1.selection.unwrap().anchor, anchor);
        });
    }

    #[test]
    fn text_replacement_clamps_selection_and_notifies() {
        let observation = Rc::new(RefCell::new(Observation::default()));
        let observation_for_controller = observation.clone();
        let recording = Recording::default();
        let id = WidgetId::next();
        let label = SelectableLabel::<String>::new()
            .controller(ProbeController::new(observation_for_controller))
            .with_id(id);
        let root = record_label(label, &recording);

        Harness::create_simple("hello world".to_owned(), root, |harness| {
            harness.send_initial_events();
            harness.just_layout();
            harness.submit_command(PROBE.with(Probe::Set(Some(Selection::new(1, 8)))).to(id));
            recording.clear();

            harness.set_data("hey".to_owned());
            let observed = observation.borrow();
            assert_eq!(observed.selection, Some(Selection::new(1, 3)));
            assert_eq!(observed.selected_text.as_deref(), Some("ey"));
            drop(observed);

            harness.event(Event::WindowConnected);
            let changes = selection_changes(&recording);
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].1.kind, SelectionChangeKind::Updated);
            assert_eq!(changes[0].1.selection, Some(Selection::new(1, 3)));
        });
    }

    #[test]
    fn text_position_mapping_respects_wrapped_wide_text() {
        let observation = Rc::new(RefCell::new(Observation::default()));
        let observation_for_controller = observation.clone();
        let recording = Recording::default();
        let id = WidgetId::next();
        let label = SelectableLabel::<String>::new()
            .controller(ProbeController::new(observation_for_controller))
            .with_id(id);
        let label = SizedBox::new(label).width(42.0).height(100.0);
        let root = record_label(label, &recording);

        Harness::create_simple("你a界 alpha beta".to_owned(), root, |harness| {
            harness.send_initial_events();
            harness.just_layout();

            for position in [0, 3, 4, 8, 9, 14] {
                harness.submit_command(PROBE.with(Probe::HitTestAt(position)).to(id));
                assert_eq!(
                    observation.borrow().position,
                    Some(position),
                    "mapping text position {position}"
                );
            }
        });
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
