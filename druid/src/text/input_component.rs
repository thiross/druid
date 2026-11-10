// Copyright 2021 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! A widget component that integrates with the platform text system.

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use tracing::instrument;

use super::{
    ArcStr, CompositeTextRange, EditableText, ImeHandlerRef, ImeInvalidation, InputHandler,
    Movement, Selection, TextAction, TextBoundary, TextLayout, TextStorage,
};
use crate::kurbo::{Line, Point, Rect, Vec2};
use crate::piet::TextLayout as _;
use crate::widget::prelude::*;
use crate::{text, theme, Cursor, Env, Modifiers, Selector, TextAlignment, UpdateCtx};

/// A widget that accepts text input.
///
/// This is intended to be used as a component of other widgets.
///
/// Text input is more complicated than you think, probably. For a good
/// overview, see [`druid_shell::text`].
///
/// This type manages an inner [`EditSession`] that is shared with the platform.
/// Unlike other aspects of Druid, the platform interacts with this session, not
/// through discrete events.
///
/// This is managed through a simple 'locking' mechanism; the platform asks for
/// a lock on a particular text session that it wishes to interact with, calls
/// methods on the locked session, and then later releases the lock.
///
/// Importantly, *other events may be received while the lock is held*.
///
/// It is the responsibility of the user of this widget to ensure that the
/// session is not locked before it is accessed. This can be done by checking
/// [`TextComponent::can_read`] and [`TextComponent::can_write`];
/// after checking these methods the inner session can be accessed via
/// [`TextComponent::borrow`] and [`TextComponent::borrow_mut`].
///
/// Sementically, this functions like a `RefCell`; attempting to borrow while
/// a lock is held will result in a panic.
#[derive(Debug, Clone)]
pub struct TextComponent<T> {
    edit_session: Arc<RefCell<EditSession<T>>>,
    lock: Arc<Cell<ImeLock>>,
    // HACK: because of the way focus works (it is managed higher up, in
    // whatever widget is controlling this) we can't rely on `is_focused` in
    // the PaintCtx.
    /// A manual flag set by the parent to control drawing behaviour.
    ///
    /// The parent should update this when handling [`LifeCycle::FocusChanged`].
    pub has_focus: bool,
}

/// A document snapshot used for generic edit transaction notifications.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextSnapshot {
    /// Contiguous display text.
    pub text: ArcStr,
    /// Composite text ranges at the time of the snapshot.
    pub composites: Vec<CompositeTextRange>,
}

impl TextSnapshot {
    pub(crate) fn from_document<T: EditableText + TextStorage>(document: &T) -> Self {
        Self {
            text: Arc::from(document.as_str()),
            composites: document.composite_ranges().to_vec(),
        }
    }
}

/// An application-observable text edit.
#[derive(Clone, Debug, PartialEq)]
pub struct TextTransaction {
    /// Monotonic transaction identifier.
    pub id: u64,
    /// Document text and composite membership before the edit.
    pub before: TextSnapshot,
    /// Document text and composite membership after the edit.
    pub after: TextSnapshot,
    /// Selection before the edit.
    pub selection_before: Selection,
    /// Selection after the edit.
    pub selection_after: Selection,
}

/// A pointer hit on a composite text range.
#[derive(Clone, Debug, PartialEq)]
pub struct CompositeHit {
    /// The hit composite.
    pub composite: CompositeTextRange,
    /// The zero-based display fragment containing the hit.
    pub fragment_index: usize,
    /// The number of display fragments occupied by the composite.
    pub fragment_count: usize,
    /// Horizontal position within the hit fragment, in the range `0..=1`.
    pub relative_x: f64,
}

/// The clipboard operation that produced a [`ClipboardEvent`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardAction {
    /// Copy the selection.
    Copy,
    /// Cut the selection.
    Cut,
}

/// Generic information about text written to the clipboard.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipboardEvent {
    /// The clipboard operation.
    pub action: ClipboardAction,
    /// The selected text range.
    pub selection: Selection,
    /// Application-neutral plain text written to the clipboard.
    pub plain_text: String,
    /// Complete composites contained in the selection.
    pub composites: Vec<CompositeTextRange>,
}

static NEXT_TRANSACTION_ID: AtomicU64 = AtomicU64::new(1);

fn next_transaction_id() -> u64 {
    NEXT_TRANSACTION_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Debug)]
struct PendingTransaction {
    before: TextSnapshot,
    selection_before: Selection,
}

/// Editable text state.
///
/// This is the inner state of a [`TextComponent`]. It should only be accessed
/// through its containing [`TextComponent`], or by the platform through an
/// [`ImeHandlerRef`] created by [`TextComponent::input_handler`].
#[derive(Debug, Clone)]
pub struct EditSession<T> {
    /// The inner [`TextLayout`] object.
    ///
    /// This is exposed so that users can do things like set text properties;
    /// you should avoid doing things like rebuilding this layout manually, or
    /// setting the text directly.
    pub layout: TextLayout<T>,
    /// If the platform modifies the text, this contains the new text;
    /// we update the app `Data` with this text on the next update pass.
    external_text_change: Option<T>,
    external_selection_change: Option<Selection>,
    external_scroll_to: Option<bool>,
    external_action: Option<TextAction>,
    /// A flag set in `update` if the text has changed from a non-IME source.
    pending_ime_invalidation: Option<ImeInvalidation>,
    /// If `true`, the component will send the [`TextComponent::RETURN`]
    /// notification when the user enters a newline.
    pub send_notification_on_return: bool,
    /// If `true`, the component will send the [`TextComponent::CANCEL`]
    /// notification when the user cancels editing.
    pub send_notification_on_cancel: bool,
    selection: Selection,
    accepts_newlines: bool,
    accepts_tabs: bool,
    alignment: TextAlignment,
    /// The y-position of the text when it does not fill our width.
    alignment_offset: f64,
    /// The portion of the text that is currently marked by the IME.
    composition_range: Option<Range<usize>>,
    drag_granularity: DragGranularity,
    /// The origin of the textbox, relative to the origin of the window.
    pub origin: Point,
    pending_transaction: Option<PendingTransaction>,
}

/// An object that can be used to acquire an `ImeHandler`.
///
/// This does not own the session; when the widget that owns the session
/// is dropped, this will become invalid.
#[derive(Debug, Clone)]
struct EditSessionRef<T> {
    inner: Weak<RefCell<EditSession<T>>>,
    lock: Arc<Cell<ImeLock>>,
}

/// A locked handle to an [`EditSession`].
///
/// This type implements [`InputHandler`]; it is the type that we pass to the
/// platform.
struct EditSessionHandle<T> {
    text: T,
    inner: Arc<RefCell<EditSession<T>>>,
}

/// When a drag follows a double- or triple-click, the behaviour of
/// drag changes to only select whole words or whole paragraphs.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DragGranularity {
    Grapheme,
    /// Keep the composite whole while dragging in either direction.
    Composite {
        start: usize,
        end: usize,
    },
    /// Start and end are the start/end bounds of the initial selection.
    Word {
        start: usize,
        end: usize,
    },
    /// Start and end are the start/end bounds of the initial selection.
    Paragraph {
        start: usize,
        end: usize,
    },
}

/// An informal lock.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ImeLock {
    None,
    ReadWrite,
    Read,
}

impl<T: TextStorage + EditableText> ImeHandlerRef for EditSessionRef<T> {
    fn is_alive(&self) -> bool {
        Weak::strong_count(&self.inner) > 0
    }

    fn acquire(&self, mutable: bool) -> Option<Box<dyn InputHandler + 'static>> {
        let lock = if mutable {
            ImeLock::ReadWrite
        } else {
            ImeLock::Read
        };
        assert_eq!(
            self.lock.replace(lock),
            ImeLock::None,
            "Ime session is already locked"
        );
        Weak::upgrade(&self.inner)
            .map(EditSessionHandle::new)
            .map(|doc| Box::new(doc) as Box<dyn InputHandler>)
    }

    fn release(&self) -> bool {
        self.lock.replace(ImeLock::None) == ImeLock::ReadWrite
    }
}

impl TextComponent<()> {
    /// A notification sent by the component when the cursor has moved.
    ///
    /// If the payload is true, this follows an edit, and the view will need
    /// layout before scrolling.
    pub const SCROLL_TO: Selector<bool> = Selector::new("druid-builtin.textbox-scroll-to");

    /// A notification sent by the component when the user hits return.
    ///
    /// This is only sent when `send_notification_on_return` is `true`.
    pub const RETURN: Selector = Selector::new("druid-builtin.textbox-return");

    /// A notification sent when the user cancels editing.
    ///
    /// This is only sent when `send_notification_on_cancel` is `true`.
    pub const CANCEL: Selector = Selector::new("druid-builtin.textbox-cancel");

    /// A notification sent by the component when the user presses the tab key.
    ///
    /// This is not sent if `accepts_tabs` is true.
    ///
    /// An ancestor can handle this event in order to do things like request
    /// a focus change.
    pub const TAB: Selector = Selector::new("druid-builtin.textbox-tab");

    /// A notification sent by the component when the user inserts a backtab.
    ///
    /// This is not sent if `accepts_tabs` is true.
    ///
    /// An ancestor can handle this event in order to do things like request
    /// a focus change.
    pub const BACKTAB: Selector = Selector::new("druid-builtin.textbox-backtab");

    /// A notification containing one completed text edit transaction.
    pub const EDIT_COMMITTED: Selector<TextTransaction> =
        Selector::new("druid-builtin.textbox-edit-committed");

    /// A notification containing generic clipboard selection information.
    pub const CLIPBOARD: Selector<ClipboardEvent> =
        Selector::new("druid-builtin.textbox-clipboard");

    /// A notification sent when the user clicks a composite text range.
    pub const COMPOSITE_HIT: Selector<CompositeHit> =
        Selector::new("druid-builtin.textbox-composite-hit");
}

impl<T> TextComponent<T> {
    /// Returns `true` if the inner [`EditSession`] can be read.
    pub fn can_read(&self) -> bool {
        self.lock.get() != ImeLock::ReadWrite
    }

    /// Returns `true` if the inner [`EditSession`] can be mutated.
    pub fn can_write(&self) -> bool {
        self.lock.get() == ImeLock::None
    }

    /// Returns `true` if the IME is actively composing (or the text is locked.)
    ///
    /// When text is composing, you should avoid doing things like modifying the
    /// selection or copy/pasting text.
    pub fn is_composing(&self) -> bool {
        self.can_read() && self.borrow().composition_range.is_some()
    }

    /// Attempt to mutably borrow the inner [`EditSession`].
    ///
    /// # Panics
    ///
    /// This method panics if there is an outstanding lock on the session.
    pub fn borrow_mut(&self) -> RefMut<'_, EditSession<T>> {
        assert!(self.can_write());
        self.edit_session.borrow_mut()
    }

    /// Attempt to borrow the inner [`EditSession`].
    ///
    /// # Panics
    ///
    /// This method panics if there is an outstanding write lock on the session.
    pub fn borrow(&self) -> Ref<'_, EditSession<T>> {
        assert!(self.can_read());
        self.edit_session.borrow()
    }
}

impl<T: EditableText + TextStorage> TextComponent<T> {
    /// Returns an [`ImeHandlerRef`] that can accept platform text input.
    ///
    /// The widget managing this component should call [`LifeCycleCtx::register_text_input`]
    /// during [`LifeCycle::WidgetAdded`], and pass it this object.
    pub fn input_handler(&self) -> impl ImeHandlerRef {
        EditSessionRef {
            inner: Arc::downgrade(&self.edit_session),
            lock: self.lock.clone(),
        }
    }
}

impl<T: TextStorage + EditableText> Widget<T> for TextComponent<T> {
    #[instrument(
        name = "InputComponent",
        level = "trace",
        skip(self, ctx, event, data, env)
    )]
    fn event(&mut self, ctx: &mut EventCtx, event: &Event, data: &mut T, env: &Env) {
        match event {
            Event::MouseDown(mouse) if self.can_write() && !ctx.is_disabled() => {
                ctx.set_active(true);
                // ensure data is up to date before a click
                let needs_rebuild = self
                    .borrow()
                    .layout
                    .text()
                    .map(|old| !old.same(data))
                    .unwrap_or(true);
                if needs_rebuild {
                    self.borrow_mut().layout.set_text(data.clone());
                    self.borrow_mut().layout.rebuild_if_needed(ctx.text(), env);
                    self.borrow_mut()
                        .update_pending_invalidation(ImeInvalidation::Reset);
                }
                let composite_hit =
                    self.borrow_mut()
                        .do_mouse_down(data, mouse.pos, mouse.mods, mouse.count);
                self.borrow_mut()
                    .update_pending_invalidation(ImeInvalidation::SelectionChanged);
                if let Some(hit) = composite_hit {
                    ctx.submit_notification_without_warning(TextComponent::COMPOSITE_HIT.with(hit));
                }
                ctx.request_update();
                ctx.request_paint();
            }
            Event::MouseMove(mouse) if self.can_write() => {
                if !ctx.is_disabled() {
                    ctx.set_cursor(&Cursor::IBeam);
                    if ctx.is_active() {
                        let pre_sel = self.borrow().selection();
                        self.borrow_mut().do_drag(data, mouse.pos);
                        if self.borrow().selection() != pre_sel {
                            self.borrow_mut()
                                .update_pending_invalidation(ImeInvalidation::SelectionChanged);
                            ctx.request_update();
                            ctx.request_paint();
                        }
                    }
                } else {
                    ctx.set_disabled(false);
                    ctx.clear_cursor();
                }
            }
            Event::MouseUp(_) if ctx.is_active() => {
                ctx.set_active(false);
                ctx.request_paint();
            }
            Event::ImeStateChange => {
                assert!(
                    self.can_write(),
                    "lock release should be cause of ImeStateChange event"
                );
                let scroll_to = self.borrow_mut().take_scroll_to();
                let action = self.borrow_mut().take_external_action();

                if let Some(scroll_to) = scroll_to {
                    ctx.submit_notification(TextComponent::SCROLL_TO.with(scroll_to));
                }
                if let Some(action) = action {
                    match action {
                        TextAction::Cancel => ctx.submit_notification(TextComponent::CANCEL),
                        TextAction::InsertNewLine { .. } => {
                            ctx.submit_notification(TextComponent::RETURN)
                        }
                        TextAction::InsertTab { .. } => ctx.submit_notification(TextComponent::TAB),
                        TextAction::InsertBacktab => {
                            ctx.submit_notification(TextComponent::BACKTAB)
                        }
                        _ => tracing::warn!("unexpected external action '{:?}'", action),
                    };
                }
                let text = self.borrow_mut().take_external_text_change();
                let selection = self.borrow_mut().take_external_selection_change();
                if let Some(text) = text {
                    self.borrow_mut().layout.set_text(text.clone());
                    *data = text;
                }
                if let Some(selection) = selection {
                    self.borrow_mut().selection = data.normalize_selection(selection);
                    ctx.request_paint();
                }
                ctx.request_update();
            }
            _ => (),
        }
    }

    #[instrument(
        name = "InputComponent",
        level = "trace",
        skip(self, ctx, event, data, env)
    )]
    fn lifecycle(&mut self, ctx: &mut LifeCycleCtx, event: &LifeCycle, data: &T, env: &Env) {
        match event {
            LifeCycle::WidgetAdded => {
                assert!(
                    self.can_write(),
                    "ime should never be locked at WidgetAdded"
                );
                self.borrow_mut().layout.set_text(data.to_owned());
                self.borrow_mut().layout.rebuild_if_needed(ctx.text(), env);
            }
            //FIXME: this should happen in the parent too?
            LifeCycle::ViewContextChanged(_) if self.can_write() => {
                if self.can_write() {
                    let prev_origin = self.borrow().origin;
                    let new_origin = ctx.window_origin();
                    if prev_origin != new_origin {
                        self.borrow_mut().origin = ctx.window_origin();
                        ctx.invalidate_text_input(ImeInvalidation::LayoutChanged);
                    }
                }
            }
            LifeCycle::DisabledChanged(disabled) => {
                if self.can_write() {
                    let color = if *disabled {
                        env.get(theme::DISABLED_TEXT_COLOR)
                    } else {
                        env.get(theme::TEXT_COLOR)
                    };

                    self.borrow_mut().layout.set_text_color(color);
                }
                ctx.request_layout();
            }
            _ => (),
        }
    }

    #[instrument(
        name = "InputComponent",
        level = "trace",
        skip(self, ctx, _old, data, env)
    )]
    fn update(&mut self, ctx: &mut UpdateCtx, _old: &T, data: &T, env: &Env) {
        if self.can_write() {
            self.borrow_mut().update(ctx, data, env);
        }
    }

    #[instrument(
        name = "InputComponent",
        level = "trace",
        skip(self, ctx, bc, _data, env)
    )]
    fn layout(&mut self, ctx: &mut LayoutCtx, bc: &BoxConstraints, _data: &T, env: &Env) -> Size {
        if !self.can_write() {
            tracing::warn!("Text layout called with IME lock held.");
            return Size::ZERO;
        }

        self.borrow_mut().layout.set_wrap_width(bc.max().width);
        self.borrow_mut().layout.rebuild_if_needed(ctx.text(), env);
        let metrics = self.borrow().layout.layout_metrics();
        let width = if bc.max().width.is_infinite() || bc.max().width < f64::MAX {
            metrics.trailing_whitespace_width
        } else {
            metrics.size.width
        };
        let size = bc.constrain((width, metrics.size.height));
        let extra_width = if self.borrow().accepts_newlines {
            0.0
        } else {
            (size.width - width).max(0.0)
        };
        self.borrow_mut().update_alignment_offset(extra_width);
        let baseline_off = metrics.size.height - metrics.first_baseline;
        ctx.set_baseline_offset(baseline_off);
        size
    }

    #[instrument(name = "InputComponent", level = "trace", skip(self, ctx, _data, env))]
    fn paint(&mut self, ctx: &mut PaintCtx, _data: &T, env: &Env) {
        if !self.can_read() {
            tracing::warn!("Text paint called with IME lock held.");
        }

        let selection_color = if self.has_focus {
            env.get(theme::SELECTED_TEXT_BACKGROUND_COLOR)
        } else {
            env.get(theme::SELECTED_TEXT_INACTIVE_BACKGROUND_COLOR)
        };

        let cursor_color = env.get(theme::CURSOR_COLOR);
        let text_offset = Vec2::new(self.borrow().alignment_offset, 0.0);

        let selection = self.borrow().selection();
        let composition = self.borrow().composition_range();
        let sel_rects = self.borrow().layout.rects_for_range(selection.range());
        if let Some(composition) = composition {
            // I believe selection should always be contained in composition range while composing?
            assert!(composition.start <= selection.anchor && composition.end >= selection.active);
            let comp_rects = self.borrow().layout.rects_for_range(composition);
            for region in comp_rects {
                let y = region.max_y().floor();
                let line = Line::new((region.min_x(), y), (region.max_x(), y)) + text_offset;
                ctx.stroke(line, &cursor_color, 1.0);
            }
            for region in sel_rects {
                let y = region.max_y().floor();
                let line = Line::new((region.min_x(), y), (region.max_x(), y)) + text_offset;
                ctx.stroke(line, &cursor_color, 2.0);
            }
        } else {
            for region in sel_rects {
                let rounded = (region + text_offset).to_rounded_rect(1.0);
                ctx.fill(rounded, &selection_color);
            }
        }
        self.borrow().layout.draw(ctx, text_offset.to_point());
    }
}

impl<T> EditSession<T> {
    /// The current [`Selection`].
    pub fn selection(&self) -> Selection {
        self.selection
    }

    /// The range of text currently being modified by an IME.
    pub fn composition_range(&self) -> Option<Range<usize>> {
        self.composition_range.clone()
    }

    /// Sets whether or not this session will allow the insertion of newlines.
    pub fn set_accepts_newlines(&mut self, accepts_newlines: bool) {
        self.accepts_newlines = accepts_newlines;
    }

    /// Set the text alignment.
    ///
    /// This is only meaningful for single-line text that does not fill
    /// the minimum layout size.
    pub fn set_text_alignment(&mut self, alignment: TextAlignment) {
        self.alignment = alignment;
    }

    /// The text alignment.
    pub fn text_alignment(&self) -> TextAlignment {
        self.alignment
    }

    /// Returns any invalidation action that should be passed to the platform.
    ///
    /// The user of this component *must* check this after calling `update`.
    pub fn pending_ime_invalidation(&mut self) -> Option<ImeInvalidation> {
        self.pending_ime_invalidation.take()
    }

    fn take_external_text_change(&mut self) -> Option<T> {
        self.external_text_change.take()
    }

    fn take_external_selection_change(&mut self) -> Option<Selection> {
        self.external_selection_change.take()
    }

    fn take_scroll_to(&mut self) -> Option<bool> {
        self.external_scroll_to.take()
    }

    fn take_external_action(&mut self) -> Option<TextAction> {
        self.external_action.take()
    }

    // we don't want to replace a more aggressive invalidation with a less aggressive one.
    fn update_pending_invalidation(&mut self, new_invalidation: ImeInvalidation) {
        self.pending_ime_invalidation = match self.pending_ime_invalidation.take() {
            None => Some(new_invalidation),
            Some(prev) => match (prev, new_invalidation) {
                (ImeInvalidation::SelectionChanged, ImeInvalidation::SelectionChanged) => {
                    ImeInvalidation::SelectionChanged
                }
                (ImeInvalidation::LayoutChanged, ImeInvalidation::LayoutChanged) => {
                    ImeInvalidation::LayoutChanged
                }
                _ => ImeInvalidation::Reset,
            }
            .into(),
        }
    }

    fn update_alignment_offset(&mut self, extra_width: f64) {
        self.alignment_offset = match self.alignment {
            TextAlignment::Start | TextAlignment::Justified => 0.0,
            TextAlignment::End => extra_width,
            TextAlignment::Center => extra_width / 2.0,
        };
    }
}

impl<T: TextStorage + EditableText> EditSession<T> {
    /// Set the selection after applying the document's composite policy.
    ///
    /// If the new selection is different from the current selection, this
    /// returns an IME invalidation that the controlling widget should apply.
    #[must_use]
    pub fn set_selection(&mut self, data: &T, selection: Selection) -> Option<ImeInvalidation> {
        let selection = data.normalize_selection(selection);
        if selection != self.selection {
            self.selection = selection;
            self.update_pending_invalidation(ImeInvalidation::SelectionChanged);
            Some(ImeInvalidation::SelectionChanged)
        } else {
            None
        }
    }

    pub(crate) fn complete_transaction(
        &mut self,
        data: &T,
        before: TextSnapshot,
        selection_before: Selection,
        was_composing: bool,
    ) -> Option<TextTransaction> {
        let after = TextSnapshot::from_document(data);
        let composing = self.composition_range.is_some();

        if before == after {
            if self.pending_transaction.is_some() && !composing {
                let pending = self.pending_transaction.take()?;
                return Some(TextTransaction {
                    id: next_transaction_id(),
                    before: pending.before,
                    after,
                    selection_before: pending.selection_before,
                    selection_after: self.selection,
                });
            }
            return None;
        }

        if composing || was_composing {
            self.pending_transaction
                .get_or_insert_with(|| PendingTransaction {
                    before: before.clone(),
                    selection_before,
                });
            if composing {
                return None;
            }
        }

        let pending = self.pending_transaction.take();
        let (before, selection_before) = pending
            .map(|pending| (pending.before, pending.selection_before))
            .unwrap_or((before, selection_before));
        Some(TextTransaction {
            id: next_transaction_id(),
            before,
            after,
            selection_before,
            selection_after: self.selection,
        })
    }

    fn normalize_hit_position(&self, data: &T, point: Point) -> usize {
        let raw = self.layout.text_position_for_point(point);
        let Some(composite) = self
            .composite_at_point(data, point)
            .or_else(|| data.composite_at(raw))
        else {
            return raw;
        };
        self.normalize_composite_position(data, &composite, point)
    }

    fn composite_at_point(&self, data: &T, point: Point) -> Option<CompositeTextRange> {
        data.composite_ranges().iter().find_map(|composite| {
            let hit = self
                .layout
                .rects_for_range(composite.range.clone())
                .into_iter()
                .any(|rect| {
                    point.x >= rect.x0
                        && point.x <= rect.x1
                        && point.y >= rect.y0
                        && point.y <= rect.y1
                });
            hit.then(|| composite.clone())
        })
    }

    fn normalize_composite_position(
        &self,
        data: &T,
        composite: &CompositeTextRange,
        point: Point,
    ) -> usize {
        let Some(index) = self.nearest_composite_fragment(composite, point) else {
            return data.normalize_caret_offset(composite.range.start, TextBoundary::Downstream);
        };
        let rects = self.layout.rects_for_range(composite.range.clone());
        let rect = rects[index];
        if point.x < rect.center().x {
            composite.range.start
        } else {
            composite.range.end
        }
    }

    fn nearest_composite_fragment(
        &self,
        composite: &CompositeTextRange,
        point: Point,
    ) -> Option<usize> {
        self.layout
            .rects_for_range(composite.range.clone())
            .into_iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                let distance = |rect: Rect| {
                    if point.y < rect.y0 {
                        rect.y0 - point.y
                    } else if point.y > rect.y1 {
                        point.y - rect.y1
                    } else {
                        0.0
                    }
                };
                distance(*a)
                    .partial_cmp(&distance(*b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(index, _)| index)
    }

    fn composite_hit(&self, data: &T, position: usize, point: Point) -> Option<CompositeHit> {
        let composite = self
            .composite_at_point(data, point)
            .or_else(|| data.composite_at(position))?;
        let rects = self.layout.rects_for_range(composite.range.clone());
        let fragment_index = self.nearest_composite_fragment(&composite, point)?;
        let rect = rects[fragment_index];
        let relative_x = if rect.width() > 0.0 {
            ((point.x - rect.x0) / rect.width()).clamp(0.0, 1.0)
        } else {
            0.5
        };
        Some(CompositeHit {
            composite,
            fragment_index,
            fragment_count: rects.len(),
            relative_x,
        })
    }

    fn normalize_edit_endpoint(
        &self,
        data: &T,
        position: usize,
        anchor: usize,
        point: Point,
    ) -> usize {
        let Some(composite) = self
            .composite_at_point(data, point)
            .or_else(|| data.composite_at(position))
        else {
            return position;
        };
        if anchor <= composite.range.start {
            composite.range.end
        } else if anchor >= composite.range.end {
            composite.range.start
        } else {
            self.normalize_composite_position(data, &composite, point)
        }
    }

    /// Insert text *not* from the IME, replacing the current selection.
    ///
    /// The caller is responsible for notifying the platform of the change in
    /// text state, by calling [`EventCtx::invalidate_text_input`].
    #[must_use]
    pub fn insert_text(&mut self, data: &mut T, new_text: &str) -> ImeInvalidation {
        let range = data.normalize_edit_range(self.selection.range());
        let new_cursor_pos = range.start + new_text.len();
        data.edit(range, new_text);
        self.selection = Selection::caret(new_cursor_pos);
        self.scroll_to_selection_end(true);
        ImeInvalidation::Reset
    }

    /// Sets the clipboard to the contents of the current selection.
    ///
    /// Returns `true` if the clipboard was set, and `false` if not (indicating)
    /// that the selection was empty.)
    pub fn set_clipboard(&self, data: &T, action: ClipboardAction) -> Option<ClipboardEvent> {
        let event = self.clipboard_event(data, action)?;
        crate::Application::global()
            .clipboard()
            .put_string(event.plain_text.clone());
        Some(event)
    }

    /// Build the application-observable clipboard event without writing to the
    /// platform clipboard.
    pub fn clipboard_event(&self, data: &T, action: ClipboardAction) -> Option<ClipboardEvent> {
        let selection = data.normalize_selection(self.selection);
        let plain_text = data.clipboard_text(selection.range()).into_owned();
        if plain_text.is_empty() {
            return None;
        }

        let composites = data
            .composite_ranges()
            .iter()
            .filter(|composite| {
                composite.range.start < selection.max() && selection.min() < composite.range.end
            })
            .cloned()
            .collect();
        let event = ClipboardEvent {
            action,
            selection,
            plain_text,
            composites,
        };
        Some(event)
    }

    fn scroll_to_selection_end(&mut self, after_edit: bool) {
        self.external_scroll_to = Some(after_edit);
    }

    fn do_action(&mut self, buffer: &mut T, action: TextAction) {
        match action {
            TextAction::Move(movement) => {
                let sel = text::movement(movement, self.selection, &self.layout, false);
                self.external_selection_change = Some(sel);
                self.scroll_to_selection_end(false);
            }
            TextAction::MoveSelecting(movement) => {
                let sel = text::movement(movement, self.selection, &self.layout, true);
                self.external_selection_change = Some(sel);
                self.scroll_to_selection_end(false);
            }
            TextAction::SelectAll => {
                let len = buffer.len();
                self.external_selection_change = Some(Selection::new(0, len));
            }
            TextAction::SelectWord => {
                if self.selection.is_caret() {
                    let range = text::movement::word_range_at(&self.layout, self.selection.active);
                    self.external_selection_change =
                        Some(buffer.normalize_selection(Selection::new(range.start, range.end)));
                }

                // it is unclear what the behaviour should be if the selection
                // is not a caret (and may span multiple words)
            }
            // This requires us to have access to the layout, which might be stale?
            TextAction::SelectLine => (),
            // this assumes our internal selection is consistent with the buffer?
            TextAction::SelectParagraph => {
                if !self.selection.is_caret() || buffer.len() < self.selection.active {
                    return;
                }
                let prev = buffer.preceding_line_break(self.selection.active);
                let next = buffer.next_line_break(self.selection.active);
                self.external_selection_change =
                    Some(buffer.normalize_selection(Selection::new(prev, next)));
            }
            TextAction::Delete(movement) if self.selection.is_caret() => {
                if movement == Movement::Grapheme(druid_shell::text::Direction::Upstream) {
                    self.backspace(buffer);
                } else {
                    let to_delete = text::movement(movement, self.selection, &self.layout, true);
                    self.selection = to_delete;
                    self.ime_insert_text(buffer, "")
                }
            }
            TextAction::Delete(_) => self.ime_insert_text(buffer, ""),
            TextAction::DecomposingBackspace => {
                tracing::warn!("Decomposing Backspace is not implemented");
                self.backspace(buffer);
            }
            //TextAction::UppercaseSelection
            //| TextAction::LowercaseSelection
            //| TextAction::TitlecaseSelection => {
            //tracing::warn!("IME transformations are not implemented");
            //}
            TextAction::InsertNewLine {
                newline_type,
                ignore_hotkey,
            } => {
                if self.send_notification_on_return && !ignore_hotkey {
                    self.external_action = Some(action);
                } else if self.accepts_newlines {
                    self.ime_insert_text(buffer, &newline_type.to_string());
                }
            }
            TextAction::InsertTab { ignore_hotkey } => {
                if ignore_hotkey || self.accepts_tabs {
                    self.ime_insert_text(buffer, "\t");
                } else if !ignore_hotkey {
                    self.external_action = Some(action);
                }
            }
            TextAction::InsertBacktab => {
                if !self.accepts_tabs {
                    self.external_action = Some(action);
                }
            }
            TextAction::InsertSingleQuoteIgnoringSmartQuotes => self.ime_insert_text(buffer, "'"),
            TextAction::InsertDoubleQuoteIgnoringSmartQuotes => self.ime_insert_text(buffer, "\""),
            TextAction::Cancel if self.send_notification_on_cancel => {
                self.external_action = Some(action)
            }
            other => tracing::warn!("unhandled IME action {:?}", other),
        }
    }

    /// Replace the current selection with `text`, and advance the cursor.
    ///
    /// This should only be called from the IME.
    fn ime_insert_text(&mut self, buffer: &mut T, text: &str) {
        let range = buffer.normalize_edit_range(self.selection.range());
        let new_cursor_pos = range.start + text.len();
        buffer.edit(range, text);
        self.external_selection_change = Some(Selection::caret(new_cursor_pos));
        self.scroll_to_selection_end(true);
    }

    fn backspace(&mut self, buffer: &mut T) {
        let to_del = if self.selection.is_caret() {
            let del_start = text::offset_for_delete_backwards(&self.selection, buffer);
            del_start..self.selection.anchor
        } else {
            self.selection.range()
        };
        let to_del = buffer.normalize_edit_range(to_del);
        self.external_selection_change = Some(Selection::caret(to_del.start));
        buffer.edit(to_del, "");
        self.scroll_to_selection_end(true);
    }

    fn do_mouse_down(
        &mut self,
        data: &T,
        point: Point,
        mods: Modifiers,
        count: u8,
    ) -> Option<CompositeHit> {
        let point = point - Vec2::new(self.alignment_offset, 0.0);
        let raw_pos = self.layout.text_position_for_point(point);
        let composite_hit = self.composite_hit(data, raw_pos, point);
        let pos = if let Some(hit) = &composite_hit {
            if hit.relative_x < 0.5 {
                hit.composite.range.start
            } else {
                hit.composite.range.end
            }
        } else {
            raw_pos
        };
        if mods.shift() {
            let active = if let Some(hit) = &composite_hit {
                if self.selection.anchor <= hit.composite.range.start {
                    hit.composite.range.end
                } else if self.selection.anchor >= hit.composite.range.end {
                    hit.composite.range.start
                } else {
                    pos
                }
            } else {
                pos
            };
            self.selection = data.normalize_selection(
                Selection::new(self.selection.anchor, active).with_h_pos(self.selection.h_pos),
            );
        } else if count == 1 {
            if let Some(hit) = &composite_hit {
                self.selection = Selection::new(hit.composite.range.start, hit.composite.range.end);
                self.drag_granularity = DragGranularity::Composite {
                    start: hit.composite.range.start,
                    end: hit.composite.range.end,
                };
            } else {
                self.selection = data.normalize_selection(Selection::caret(pos));
                self.drag_granularity = DragGranularity::Grapheme;
            }
        } else if let Some(hit) = &composite_hit {
            self.selection = Selection::new(hit.composite.range.start, hit.composite.range.end);
            self.drag_granularity = DragGranularity::Composite {
                start: hit.composite.range.start,
                end: hit.composite.range.end,
            };
        } else {
            let Range { start, end } = self.sel_region_for_pos(data, raw_pos, count);
            self.selection = data.normalize_selection(Selection::new(start, end));
            self.drag_granularity = match count {
                2 => DragGranularity::Word { start, end },
                3 => DragGranularity::Paragraph { start, end },
                _ => DragGranularity::Grapheme,
            };
        }
        composite_hit
    }

    fn do_drag(&mut self, data: &T, point: Point) {
        let point = point - Vec2::new(self.alignment_offset, 0.0);
        //FIXME: this should behave differently if we were double or triple clicked
        let raw_pos = self.layout.text_position_for_point(point);
        let pos = self.normalize_edit_endpoint(data, raw_pos, self.selection.anchor, point);
        let text = match self.layout.text() {
            Some(text) => text,
            None => return,
        };

        let (start, end) = match self.drag_granularity {
            DragGranularity::Grapheme => (self.selection.anchor, pos),
            DragGranularity::Composite { start, end } => {
                if pos < start {
                    (pos, end)
                } else if pos > end {
                    (start, pos)
                } else {
                    (start, end)
                }
            }
            DragGranularity::Word { start, end } => {
                let word_range = self.word_for_pos(pos);
                if pos <= start {
                    (end, word_range.start)
                } else {
                    (start, word_range.end)
                }
            }
            DragGranularity::Paragraph { start, end } => {
                let par_start = text.preceding_line_break(pos);
                let par_end = text.next_line_break(pos);

                if pos <= start {
                    (end, par_start)
                } else {
                    (start, par_end)
                }
            }
        };
        self.selection = data.normalize_selection(Selection::new(start, end));
        self.scroll_to_selection_end(false);
    }

    /// Returns a line suitable for drawing a standard cursor.
    pub fn cursor_line_for_text_position(&self, pos: usize) -> Line {
        let line = self.layout.cursor_line_for_text_position(pos);
        line + Vec2::new(self.alignment_offset, 0.0)
    }

    fn sel_region_for_pos(&mut self, data: &T, pos: usize, click_count: u8) -> Range<usize> {
        match click_count {
            1 => pos..pos,
            2 => {
                let range = self.word_for_pos(pos);
                data.normalize_selection(Selection::new(range.start, range.end))
                    .range()
            }
            _ => {
                let text = match self.layout.text() {
                    Some(text) => text,
                    None => return pos..pos,
                };
                let line_min = text.preceding_line_break(pos);
                let line_max = text.next_line_break(pos);
                data.normalize_selection(Selection::new(line_min, line_max))
                    .range()
            }
        }
    }

    fn word_for_pos(&self, pos: usize) -> Range<usize> {
        text::movement::word_range_at(&self.layout, pos)
    }

    fn update(&mut self, ctx: &mut UpdateCtx, new_data: &T, env: &Env) {
        if let Err(error) = new_data.validate_composite_ranges() {
            debug_assert!(
                false,
                "invalid composite text ranges in {}: {error:?}",
                std::any::type_name::<T>()
            );
            tracing::error!(
                document = std::any::type_name::<T>(),
                ?error,
                "invalid composite text ranges"
            );
        }
        if self
            .layout
            .text()
            .as_ref()
            .map(|t| !t.same(new_data))
            .unwrap_or(true)
        {
            self.update_pending_invalidation(ImeInvalidation::Reset);
            self.layout.set_text(new_data.clone());
        }
        if self.layout.needs_rebuild_after_update(ctx) {
            ctx.request_layout();
        }
        let new_sel = new_data.normalize_selection(self.selection.constrained(new_data.as_str()));
        if new_sel != self.selection {
            self.selection = new_sel;
            self.update_pending_invalidation(ImeInvalidation::SelectionChanged);
        }
        self.layout.rebuild_if_needed(ctx.text(), env);
    }
}

impl<T: TextStorage> EditSessionHandle<T> {
    fn new(inner: Arc<RefCell<EditSession<T>>>) -> Self {
        let text = inner.borrow().layout.text().cloned().unwrap();
        EditSessionHandle { text, inner }
    }
}

impl<T: TextStorage + EditableText> InputHandler for EditSessionHandle<T> {
    fn selection(&self) -> Selection {
        self.inner.borrow().selection
    }

    fn set_selection(&mut self, selection: Selection) {
        self.inner.borrow_mut().external_selection_change =
            Some(self.text.normalize_selection(selection));
        self.inner.borrow_mut().external_scroll_to = Some(true);
    }

    fn composition_range(&self) -> Option<Range<usize>> {
        self.inner.borrow().composition_range.clone()
    }

    fn set_composition_range(&mut self, range: Option<Range<usize>>) {
        let Some(range) = range else {
            self.inner.borrow_mut().composition_range = None;
            return;
        };

        let overlaps = if range.is_empty() {
            self.text
                .composite_ranges()
                .iter()
                .any(|composite| composite.strictly_contains(range.start))
        } else {
            self.text.composite_ranges().iter().any(|composite| {
                composite.range.start < range.end && range.start < composite.range.end
            })
        };
        if overlaps {
            let normalized = self.text.normalize_edit_range(range);
            let mut inner = self.inner.borrow_mut();
            inner.composition_range = None;
            inner.external_selection_change = Some(Selection::caret(normalized.start));
            inner.external_scroll_to = Some(true);
            inner.update_pending_invalidation(ImeInvalidation::Reset);
        } else {
            self.inner.borrow_mut().composition_range = Some(range);
        }
    }

    fn is_char_boundary(&self, i: usize) -> bool {
        self.text.cursor(i).is_some()
    }

    fn len(&self) -> usize {
        self.text.len()
    }

    fn slice(&self, range: Range<usize>) -> std::borrow::Cow<'_, str> {
        self.text.slice(range).unwrap()
    }

    fn replace_range(&mut self, range: Range<usize>, text: &str) {
        let range = self.text.normalize_edit_range(range);
        self.text.edit(range, text);
        self.inner.borrow_mut().external_text_change = Some(self.text.clone());
    }

    fn hit_test_point(&self, point: Point) -> crate::piet::HitTestPoint {
        let inner = self.inner.borrow();
        let mut hit = inner
            .layout
            .layout()
            .map(|layout| layout.hit_test_point(point))
            .unwrap_or_default();
        hit.idx = inner.normalize_hit_position(&self.text, point);
        hit
    }

    fn line_range(&self, index: usize, _affinity: druid_shell::text::Affinity) -> Range<usize> {
        let inner = self.inner.borrow();
        let layout = inner.layout.layout().unwrap();
        let hit = layout.hit_test_text_position(index);
        let metric = layout.line_metric(hit.line).unwrap();
        metric.range()
    }

    fn bounding_box(&self) -> Option<Rect> {
        let size = self.inner.borrow().layout.size();
        Some(Rect::from_origin_size(self.inner.borrow().origin, size))
    }

    fn slice_bounding_box(&self, range: Range<usize>) -> Option<Rect> {
        let origin = self.inner.borrow().origin;
        let layout = &self.inner.borrow().layout;
        if range.is_empty() {
            let hit = layout
                .layout()
                .map(|l| l.hit_test_text_position(range.start))?;
            let line = layout.layout().and_then(|l| l.line_metric(hit.line))?;
            let x = hit.point.x;
            Some(Rect::new(x, line.y_offset, x, line.y_offset + line.height))
        } else {
            layout.rects_for_range(range).first().copied()
        }
        .map(|rect| rect + origin.to_vec2())
    }

    fn handle_action(&mut self, action: TextAction) {
        self.inner.borrow_mut().do_action(&mut self.text, action);
        let text_changed = self
            .inner
            .borrow()
            .layout
            .text()
            .map(|old| !old.same(&self.text))
            .unwrap_or(true);
        if text_changed {
            self.inner.borrow_mut().external_text_change = Some(self.text.clone());
        }
    }
}

impl<T> Default for TextComponent<T> {
    fn default() -> Self {
        let inner = EditSession {
            layout: TextLayout::new(),
            external_scroll_to: None,
            external_text_change: None,
            external_selection_change: None,
            external_action: None,
            pending_ime_invalidation: None,
            selection: Selection::caret(0),
            composition_range: None,
            send_notification_on_return: false,
            send_notification_on_cancel: false,
            accepts_newlines: false,
            accepts_tabs: false,
            alignment: TextAlignment::Start,
            alignment_offset: 0.0,
            drag_granularity: DragGranularity::Grapheme,
            origin: Point::ZERO,
            pending_transaction: None,
        };

        TextComponent {
            edit_session: Arc::new(RefCell::new(inner)),
            // TODO: Figure out if this needs to stay Arc, or if it can be switched to Rc
            #[allow(clippy::arc_with_non_send_sync)]
            lock: Arc::new(Cell::new(ImeLock::None)),
            has_focus: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::piet::TextStorage as PietTextStorage;
    use crate::tests::harness::Harness;
    use crate::text::{CompositeKey, StringCursor};
    use crate::widget::{Controller, TextBox, WidgetExt};
    use crate::{Data, Env};

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct CompositeDocument {
        text: String,
        composites: Vec<CompositeTextRange>,
    }

    impl CompositeDocument {
        fn new() -> Self {
            Self::with_composite_text("ab<label>cd", 2..9)
        }

        fn with_composite_text(text: &str, range: Range<usize>) -> Self {
            Self {
                text: text.to_owned(),
                composites: vec![CompositeTextRange::new(CompositeKey::new(7), range)],
            }
        }
    }

    impl Data for CompositeDocument {
        fn same(&self, other: &Self) -> bool {
            self == other
        }
    }

    impl PietTextStorage for CompositeDocument {
        fn as_str(&self) -> &str {
            &self.text
        }
    }

    impl TextStorage for CompositeDocument {}

    impl EditableText for CompositeDocument {
        fn cursor(&self, position: usize) -> Option<StringCursor<'_>> {
            self.text.cursor(position)
        }

        fn edit(&mut self, range: Range<usize>, new: impl Into<String>) {
            self.text.edit(range, new);
        }

        fn slice(&self, range: Range<usize>) -> Option<std::borrow::Cow<'_, str>> {
            self.text.slice(range)
        }

        fn len(&self) -> usize {
            self.text.len()
        }

        fn prev_word_offset(&self, offset: usize) -> Option<usize> {
            self.text.prev_word_offset(offset)
        }

        fn next_word_offset(&self, offset: usize) -> Option<usize> {
            self.text.next_word_offset(offset)
        }

        fn prev_grapheme_offset(&self, offset: usize) -> Option<usize> {
            self.text.prev_grapheme_offset(offset)
        }

        fn next_grapheme_offset(&self, offset: usize) -> Option<usize> {
            self.text.next_grapheme_offset(offset)
        }

        fn prev_codepoint_offset(&self, offset: usize) -> Option<usize> {
            self.text.prev_codepoint_offset(offset)
        }

        fn next_codepoint_offset(&self, offset: usize) -> Option<usize> {
            self.text.next_codepoint_offset(offset)
        }

        fn preceding_line_break(&self, offset: usize) -> usize {
            self.text.preceding_line_break(offset)
        }

        fn next_line_break(&self, offset: usize) -> usize {
            self.text.next_line_break(offset)
        }

        fn is_empty(&self) -> bool {
            self.text.is_empty()
        }

        fn from_str(s: &str) -> Self {
            Self {
                text: s.to_owned(),
                composites: Vec::new(),
            }
        }

        fn composite_ranges(&self) -> &[CompositeTextRange] {
            &self.composites
        }

        fn composite_text(&self, _composite: &CompositeTextRange) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Borrowed("<reference>")
        }
    }

    fn before(data: &CompositeDocument) -> TextSnapshot {
        TextSnapshot::from_document(data)
    }

    fn component_with_document() -> TextComponent<CompositeDocument> {
        let component = TextComponent::<CompositeDocument>::default();
        component
            .borrow_mut()
            .layout
            .set_text(CompositeDocument::new());
        component
    }

    #[test]
    fn completed_edit_emits_transaction_snapshot() {
        let mut data = CompositeDocument::new();
        let before = before(&data);
        let selection_before = Selection::caret(2);
        data.edit(2..2, "x");
        let component = TextComponent::<CompositeDocument>::default();
        let mut session = component.borrow_mut();
        session.selection = Selection::caret(3);

        let transaction = session
            .complete_transaction(&data, before, selection_before, false)
            .expect("edit should produce a transaction");
        assert_eq!(transaction.before.text.as_ref(), "ab<label>cd");
        assert_eq!(transaction.after.text.as_ref(), "abx<label>cd");
        assert_eq!(transaction.selection_before, Selection::caret(2));
        assert_eq!(transaction.selection_after, Selection::caret(3));
    }

    #[test]
    fn ime_edits_are_coalesced_into_one_transaction() {
        let mut data = CompositeDocument::new();
        let first_before = before(&data);
        let component = TextComponent::<CompositeDocument>::default();
        let mut session = component.borrow_mut();
        session.selection = Selection::caret(0);
        session.composition_range = Some(0..0);

        data.edit(0..0, "a");
        assert!(session
            .complete_transaction(&data, first_before, Selection::caret(0), false)
            .is_none());

        let second_before = before(&data);
        session.selection = Selection::caret(1);
        data.edit(1..1, "b");
        session.composition_range = None;
        let transaction = session
            .complete_transaction(&data, second_before, Selection::caret(1), true)
            .expect("finished composition should emit one transaction");

        assert_eq!(transaction.before.text.as_ref(), "ab<label>cd");
        assert_eq!(transaction.after.text.as_ref(), "abab<label>cd");
        assert_eq!(transaction.selection_before, Selection::caret(0));
        assert_eq!(transaction.selection_after, Selection::caret(1));
    }

    #[test]
    fn composition_range_cannot_span_a_composite() {
        let component = component_with_document();
        let handler = component.input_handler();
        let mut handler = handler.acquire(true).unwrap();

        handler.set_composition_range(Some(4..6));
        assert_eq!(handler.composition_range(), None);
    }

    #[test]
    fn composition_range_adjacent_to_composite_is_accepted() {
        let component = component_with_document();
        let handler = component.input_handler();
        let mut handler = handler.acquire(true).unwrap();

        handler.set_composition_range(Some(0..2));
        assert_eq!(handler.composition_range(), Some(0..2));

        handler.set_composition_range(Some(2..2));
        assert_eq!(handler.composition_range(), Some(2..2));
    }

    #[test]
    fn input_replacement_expands_to_the_composite() {
        let component = component_with_document();
        let handler = component.input_handler();
        let mut handler = handler.acquire(true).unwrap();

        handler.replace_range(4..6, "x");
        assert_eq!(handler.slice(0..handler.len()).as_ref(), "abxcd");
    }

    #[test]
    fn insertion_at_composite_boundary_keeps_the_composite() {
        let component = TextComponent::<CompositeDocument>::default();
        let mut session = component.borrow_mut();
        let mut data = CompositeDocument::new();
        session.selection = Selection::caret(2);

        let _ = session.insert_text(&mut data, "x");
        assert_eq!(data.text, "abx<label>cd");
    }

    #[test]
    fn insertion_inside_composite_replaces_it_whole() {
        let component = TextComponent::<CompositeDocument>::default();
        let mut session = component.borrow_mut();
        let mut data = CompositeDocument::new();
        session.selection = Selection::caret(4);

        let _ = session.insert_text(&mut data, "x");
        assert_eq!(data.text, "abxcd");
    }

    #[test]
    fn backspace_after_composite_removes_it_whole() {
        let component = TextComponent::<CompositeDocument>::default();
        let mut session = component.borrow_mut();
        let mut data = CompositeDocument::new();
        session.selection = Selection::caret(9);

        session.backspace(&mut data);
        assert_eq!(data.text, "abcd");
    }

    #[test]
    fn copy_event_uses_composite_plain_text() {
        let component = TextComponent::<CompositeDocument>::default();
        let mut session = component.borrow_mut();
        let data = CompositeDocument::new();
        session.selection = Selection::new(0, 11);

        let event = session
            .clipboard_event(&data, ClipboardAction::Copy)
            .expect("nonempty selection should produce clipboard data");
        assert_eq!(event.plain_text, "ab<reference>cd");
        assert_eq!(event.composites, data.composites);
    }

    #[cfg(feature = "tui")]
    struct SelectionProbe(Rc<Cell<Selection>>);

    #[cfg(feature = "tui")]
    impl Controller<CompositeDocument, TextComponent<CompositeDocument>> for SelectionProbe {
        fn event(
            &mut self,
            child: &mut TextComponent<CompositeDocument>,
            ctx: &mut EventCtx,
            event: &Event,
            data: &mut CompositeDocument,
            env: &Env,
        ) {
            child.event(ctx, event, data, env);
            if matches!(event, Event::MouseDown(_) | Event::MouseMove(_)) {
                self.0.set(child.borrow().selection());
            }
        }
    }

    #[cfg(feature = "tui")]
    #[test]
    fn clicking_inside_composite_selects_it_whole() {
        let observed = Rc::new(Cell::new(Selection::default()));
        let probe = SelectionProbe(observed.clone());
        let widget = TextComponent::<CompositeDocument>::default().controller(probe);

        Harness::create_simple(CompositeDocument::new(), widget, |harness| {
            harness.send_initial_events();
            harness.just_layout();
            harness.event(Event::MouseDown(crate::MouseEvent {
                pos: Point::new(71.0, 8.0),
                window_pos: Point::new(71.0, 8.0),
                buttons: crate::MouseButtons::new(),
                mods: crate::Modifiers::default(),
                count: 1,
                focus: true,
                button: crate::MouseButton::Left,
                wheel_delta: Vec2::ZERO,
            }));
            assert_eq!(observed.get(), Selection::new(2, 9));
        });
    }

    #[cfg(feature = "tui")]
    #[test]
    fn dragging_away_from_composite_keeps_it_selected() {
        let observed = Rc::new(Cell::new(Selection::default()));
        let probe = SelectionProbe(observed.clone());
        let widget = TextComponent::<CompositeDocument>::default().controller(probe);

        Harness::create_simple(CompositeDocument::new(), widget, |harness| {
            harness.send_initial_events();
            harness.just_layout();
            harness.event(Event::MouseDown(crate::MouseEvent {
                pos: Point::new(33.0, 8.0),
                window_pos: Point::new(33.0, 8.0),
                buttons: crate::MouseButtons::new(),
                mods: crate::Modifiers::default(),
                count: 1,
                focus: true,
                button: crate::MouseButton::Left,
                wheel_delta: Vec2::ZERO,
            }));
            harness.event(Event::MouseMove(crate::MouseEvent {
                pos: Point::new(1.0, 8.0),
                window_pos: Point::new(1.0, 8.0),
                buttons: crate::MouseButtons::new().with(crate::MouseButton::Left),
                mods: crate::Modifiers::default(),
                count: 0,
                focus: false,
                button: crate::MouseButton::None,
                wheel_delta: Vec2::ZERO,
            }));
            assert_eq!(observed.get(), Selection::new(0, 9));
        });
    }

    #[cfg(feature = "tui")]
    #[test]
    fn double_clicking_composite_selects_it_whole() {
        let observed = Rc::new(Cell::new(Selection::default()));
        let probe = SelectionProbe(observed.clone());
        let widget = TextComponent::<CompositeDocument>::default().controller(probe);

        Harness::create_simple(CompositeDocument::new(), widget, |harness| {
            harness.send_initial_events();
            harness.just_layout();
            harness.event(Event::MouseDown(crate::MouseEvent {
                pos: Point::new(33.0, 8.0),
                window_pos: Point::new(33.0, 8.0),
                buttons: crate::MouseButtons::new(),
                mods: crate::Modifiers::default(),
                count: 2,
                focus: true,
                button: crate::MouseButton::Left,
                wheel_delta: Vec2::ZERO,
            }));
            assert_eq!(observed.get(), Selection::new(2, 9));
        });
    }

    #[cfg(feature = "tui")]
    #[test]
    fn clicking_wrapped_composite_fragment_selects_it_whole() {
        let observed = Rc::new(Cell::new(Selection::default()));
        let probe = SelectionProbe(observed.clone());
        let widget = TextComponent::<CompositeDocument>::default().controller(probe);
        let data = CompositeDocument::with_composite_text("xxabcdefghijklYY", 2..14);

        Harness::create_with_render(
            data,
            widget,
            crate::Size::new(80.0, 80.0),
            |harness| {
                harness.send_initial_events();
                harness.just_layout();
                harness.event(Event::MouseDown(crate::MouseEvent {
                    pos: Point::new(20.0, 24.0),
                    window_pos: Point::new(20.0, 24.0),
                    buttons: crate::MouseButtons::new(),
                    mods: crate::Modifiers::default(),
                    count: 1,
                    focus: true,
                    button: crate::MouseButton::Left,
                    wheel_delta: Vec2::ZERO,
                }));
                assert_eq!(observed.get(), Selection::new(2, 14));
            },
            |_| {},
        );
    }

    struct MovementProbe(Rc<Cell<Option<Selection>>>);

    impl Controller<CompositeDocument, TextBox<CompositeDocument>> for MovementProbe {
        fn event(
            &mut self,
            child: &mut TextBox<CompositeDocument>,
            ctx: &mut EventCtx,
            event: &Event,
            data: &mut CompositeDocument,
            env: &Env,
        ) {
            if matches!(event, Event::WindowSize(_)) {
                let session = child.text().borrow();
                self.0.set(Some(text::movement(
                    Movement::Grapheme(crate::text::Direction::Downstream),
                    Selection::caret(2),
                    &session.layout,
                    false,
                )));
            }
            child.event(ctx, event, data, env);
        }
    }

    #[test]
    fn grapheme_movement_crosses_composite_as_one_unit() {
        let observed = Rc::new(Cell::new(None));
        let widget =
            TextBox::<CompositeDocument>::new().controller(MovementProbe(observed.clone()));

        Harness::create_simple(CompositeDocument::new(), widget, |harness| {
            harness.send_initial_events();
            harness.just_layout();
            harness.event(Event::WindowSize(crate::Size::new(400.0, 400.0)));
            assert_eq!(observed.get(), Some(Selection::caret(9)));
        });
    }
}
