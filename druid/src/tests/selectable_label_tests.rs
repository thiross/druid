// Copyright 2026 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use crate::tests::harness::Harness;
use crate::tests::move_mouse;
use crate::text::Selection;
use crate::widget::{Axis, SelectableLabel, SelectionChange};
use crate::{
    BoxConstraints, Data, Env, Event, EventCtx, LayoutCtx, LifeCycle, LifeCycleCtx, Modifiers,
    MouseButton, MouseButtons, MouseEvent, PaintCtx, Point, Rect, Selector, Size, UpdateCtx,
    Widget, WidgetId, WidgetPod,
};

const SNAPSHOT: Selector = Selector::new("druid-tests.selectable-label-snapshot");

#[derive(Clone, PartialEq, Eq)]
struct TranscriptData {
    messages: Vec<String>,
}

impl Data for TranscriptData {
    fn same(&self, other: &Self) -> bool {
        self == other
    }
}

#[derive(Clone, Debug, Default)]
struct TranscriptSnapshot {
    selections: Vec<Option<Selection>>,
    ranges: Vec<Option<Range<usize>>>,
    fragments: Vec<Option<String>>,
    copied_text: String,
    rects: Vec<Rect>,
    changes: Vec<(WidgetId, SelectionChange)>,
}

struct MessageLabel {
    index: usize,
    label: SelectableLabel<String>,
}

impl Widget<TranscriptData> for MessageLabel {
    fn event(&mut self, ctx: &mut EventCtx, event: &Event, data: &mut TranscriptData, env: &Env) {
        self.label
            .event(ctx, event, &mut data.messages[self.index], env);
    }

    fn lifecycle(
        &mut self,
        ctx: &mut LifeCycleCtx,
        event: &LifeCycle,
        data: &TranscriptData,
        env: &Env,
    ) {
        self.label
            .lifecycle(ctx, event, &data.messages[self.index], env);
    }

    fn update(
        &mut self,
        ctx: &mut UpdateCtx,
        old_data: &TranscriptData,
        data: &TranscriptData,
        env: &Env,
    ) {
        self.label.update(
            ctx,
            &old_data.messages[self.index],
            &data.messages[self.index],
            env,
        );
    }

    fn layout(
        &mut self,
        ctx: &mut LayoutCtx,
        bc: &BoxConstraints,
        data: &TranscriptData,
        env: &Env,
    ) -> Size {
        self.label.layout(ctx, bc, &data.messages[self.index], env)
    }

    fn paint(&mut self, ctx: &mut PaintCtx, data: &TranscriptData, env: &Env) {
        self.label.paint(ctx, &data.messages[self.index], env);
    }

    fn compute_max_intrinsic(
        &mut self,
        axis: Axis,
        ctx: &mut LayoutCtx,
        bc: &BoxConstraints,
        data: &TranscriptData,
        env: &Env,
    ) -> f64 {
        self.label
            .compute_max_intrinsic(axis, ctx, bc, &data.messages[self.index], env)
    }
}

struct Transcript {
    labels: Vec<WidgetPod<TranscriptData, MessageLabel>>,
    anchor: Option<(usize, usize)>,
    active: Option<(usize, usize)>,
    dragging: bool,
    snapshot: Rc<RefCell<TranscriptSnapshot>>,
}

impl Transcript {
    fn new(messages: &[String], snapshot: Rc<RefCell<TranscriptSnapshot>>) -> Self {
        let labels = messages
            .iter()
            .enumerate()
            .map(|(index, _)| {
                WidgetPod::new(MessageLabel {
                    index,
                    label: SelectableLabel::new(),
                })
            })
            .collect();
        Self {
            labels,
            anchor: None,
            active: None,
            dragging: false,
            snapshot,
        }
    }

    fn capture(&self) {
        let mut snapshot = self.snapshot.borrow_mut();
        snapshot.selections.clear();
        snapshot.ranges.clear();
        snapshot.fragments.clear();
        snapshot.rects.clear();

        for pod in &self.labels {
            let label = &pod.widget().label;
            snapshot.selections.push(label.selection());
            snapshot.ranges.push(label.selected_range());
            snapshot
                .fragments
                .push(label.selected_text().map(|text| text.to_string()));
            snapshot.rects.push(pod.layout_rect());
        }
        snapshot.copied_text = snapshot
            .fragments
            .iter()
            .filter_map(|fragment| fragment.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
    }

    fn label_index_at(&self, point: Point) -> Option<usize> {
        if self.labels.is_empty() {
            return None;
        }
        self.labels
            .iter()
            .position(|label| point.y < label.layout_rect().y1)
            .or_else(|| self.labels.len().checked_sub(1))
    }

    fn position_at(&self, index: usize, point: Point) -> usize {
        let label = &self.labels[index];
        let local_point = point - label.layout_rect().origin().to_vec2();
        label
            .widget()
            .label
            .text_position_for_point(local_point)
            .unwrap_or_default()
    }

    fn set_label_selection(&self, ctx: &mut EventCtx, index: usize, selection: Option<Selection>) {
        ctx.submit_command(
            SelectableLabel::<String>::SET_SELECTION
                .with(selection)
                .to(self.labels[index].id()),
        );
    }

    fn apply_span(&self, ctx: &mut EventCtx, data: &TranscriptData) {
        let (Some(anchor), Some(active)) = (self.anchor, self.active) else {
            return;
        };
        let start = anchor.0.min(active.0);
        let end = anchor.0.max(active.0);
        let start_offset = if anchor.0 <= active.0 {
            anchor.1
        } else {
            active.1
        };
        let end_offset = if anchor.0 <= active.0 {
            active.1
        } else {
            anchor.1
        };

        for index in 0..self.labels.len() {
            let selection = if start == end {
                (index == start).then(|| Selection::new(start_offset, end_offset))
            } else if index == start {
                Some(Selection::new(start_offset, data.messages[index].len()))
            } else if index == end {
                Some(Selection::new(0, end_offset))
            } else if (start..end).contains(&index) {
                Some(Selection::new(0, data.messages[index].len()))
            } else {
                None
            };
            self.set_label_selection(ctx, index, selection);
        }
    }

    fn handle_mouse(
        &mut self,
        ctx: &mut EventCtx,
        mouse: &MouseEvent,
        data: &TranscriptData,
        begin: bool,
    ) {
        let Some(index) = self.label_index_at(mouse.pos) else {
            return;
        };
        let position = self.position_at(index, mouse.pos);
        self.active = Some((index, position));
        if begin {
            if !mouse.mods.shift() {
                self.anchor = Some((index, position));
            }
            self.dragging = true;
        }
        self.apply_span(ctx, data);
    }
}

impl Widget<TranscriptData> for Transcript {
    fn event(&mut self, ctx: &mut EventCtx, event: &Event, data: &mut TranscriptData, env: &Env) {
        if let Event::Notification(notification) = event {
            if notification.is(SelectableLabel::<String>::SELECTION_CHANGED) {
                self.snapshot.borrow_mut().changes.push((
                    notification.source(),
                    *notification
                        .get(SelectableLabel::<String>::SELECTION_CHANGED)
                        .unwrap(),
                ));
                ctx.set_handled();
            }
            return;
        }
        if let Event::Command(command) = event {
            if command.is(SNAPSHOT) {
                self.capture();
                ctx.set_handled();
                return;
            }
        }

        for label in &mut self.labels {
            label.event(ctx, event, data, env);
        }

        match event {
            Event::MouseDown(mouse) if mouse.button == MouseButton::Left && !ctx.is_disabled() => {
                self.handle_mouse(ctx, mouse, data, true);
            }
            Event::MouseMove(mouse) if self.dragging && !ctx.is_disabled() => {
                self.handle_mouse(ctx, mouse, data, false);
            }
            Event::MouseUp(mouse) if mouse.button == MouseButton::Left && self.dragging => {
                self.handle_mouse(ctx, mouse, data, false);
                self.dragging = false;
                self.capture();
            }
            _ => {}
        }
    }

    fn lifecycle(
        &mut self,
        ctx: &mut LifeCycleCtx,
        event: &LifeCycle,
        data: &TranscriptData,
        env: &Env,
    ) {
        for label in &mut self.labels {
            label.lifecycle(ctx, event, data, env);
        }
    }

    fn update(
        &mut self,
        ctx: &mut UpdateCtx,
        _old_data: &TranscriptData,
        data: &TranscriptData,
        env: &Env,
    ) {
        for label in &mut self.labels {
            label.update(ctx, data, env);
        }
        if let Some((index, position)) = &mut self.anchor {
            *position = (*position).min(data.messages[*index].len());
        }
        if let Some((index, position)) = &mut self.active {
            *position = (*position).min(data.messages[*index].len());
        }
    }

    fn layout(
        &mut self,
        ctx: &mut LayoutCtx,
        bc: &BoxConstraints,
        data: &TranscriptData,
        env: &Env,
    ) -> Size {
        let mut y = 0.0;
        let mut width: f64 = 0.0;
        for label in &mut self.labels {
            let size = label.layout(ctx, &bc.loosen(), data, env);
            label.set_origin(ctx, Point::new(0.0, y));
            y += size.height;
            width = width.max(size.width);
        }
        bc.constrain(Size::new(width, y))
    }

    fn paint(&mut self, ctx: &mut PaintCtx, data: &TranscriptData, env: &Env) {
        for label in &mut self.labels {
            label.paint(ctx, data, env);
        }
    }
}

fn mouse_event(pos: Point, button: MouseButton, mods: Modifiers) -> MouseEvent {
    let mut event = move_mouse(pos);
    event.button = button;
    event.mods = mods;
    event.buttons = match button {
        MouseButton::Left => MouseButtons::new().with(MouseButton::Left),
        _ => MouseButtons::new(),
    };
    event
}

fn text_point(snapshot: &TranscriptSnapshot, index: usize, x: f64) -> Point {
    let rect = snapshot.rects[index];
    Point::new(x, rect.center().y)
}

fn snapshot(
    harness: &mut Harness<TranscriptData>,
    observed: &Rc<RefCell<TranscriptSnapshot>>,
) -> TranscriptSnapshot {
    harness.submit_command(SNAPSHOT);
    observed.borrow().clone()
}

fn create_transcript(
    messages: Vec<String>,
) -> (Transcript, Rc<RefCell<TranscriptSnapshot>>, TranscriptData) {
    let snapshot = Rc::new(RefCell::new(TranscriptSnapshot::default()));
    let transcript = Transcript::new(&messages, snapshot.clone());
    let data = TranscriptData { messages };
    (transcript, snapshot, data)
}

#[test]
fn starting_selection_in_second_label_clears_the_first() {
    let messages = vec!["alpha".to_owned(), "bravo".to_owned()];
    let (transcript, observed, data) = create_transcript(messages);

    Harness::create_simple(data, transcript, |harness| {
        harness.send_initial_events();
        harness.just_layout();
        let geometry = snapshot(harness, &observed);

        harness.event(Event::MouseDown(mouse_event(
            text_point(&geometry, 0, 0.0),
            MouseButton::Left,
            Modifiers::default(),
        )));
        harness.event(Event::MouseMove(move_mouse(text_point(
            &geometry, 0, 300.0,
        ))));
        harness.event(Event::MouseUp(mouse_event(
            text_point(&geometry, 0, 300.0),
            MouseButton::Left,
            Modifiers::default(),
        )));

        let first_selected = snapshot(harness, &observed);
        assert_eq!(first_selected.fragments[0].as_deref(), Some("alpha"));
        assert_eq!(
            first_selected.ranges.iter().flatten().count(),
            1,
            "only label A should be visibly selected"
        );

        harness.event(Event::MouseDown(mouse_event(
            text_point(&geometry, 1, 0.0),
            MouseButton::Left,
            Modifiers::default(),
        )));
        harness.event(Event::MouseUp(mouse_event(
            text_point(&geometry, 1, 0.0),
            MouseButton::Left,
            Modifiers::default(),
        )));

        let second_started = snapshot(harness, &observed);
        assert_eq!(second_started.fragments[0], None);
        assert_eq!(
            second_started.ranges.iter().flatten().count(),
            0,
            "starting a caret selection in label B must clear label A"
        );
    });
}

#[test]
fn dragging_between_labels_selects_and_copies_in_application_order() {
    let messages = vec!["alpha".to_owned(), "bravo".to_owned()];
    let (transcript, observed, data) = create_transcript(messages);

    Harness::create_simple(data, transcript, |harness| {
        harness.send_initial_events();
        harness.just_layout();
        let geometry = snapshot(harness, &observed);

        harness.event(Event::MouseDown(mouse_event(
            text_point(&geometry, 0, 0.0),
            MouseButton::Left,
            Modifiers::default(),
        )));
        harness.event(Event::MouseMove(move_mouse(text_point(
            &geometry, 1, 300.0,
        ))));
        harness.event(Event::MouseUp(mouse_event(
            text_point(&geometry, 1, 300.0),
            MouseButton::Left,
            Modifiers::default(),
        )));

        let selected = snapshot(harness, &observed);
        assert_eq!(selected.fragments[0].as_deref(), Some("alpha"));
        assert_eq!(selected.fragments[1].as_deref(), Some("bravo"));
        assert_eq!(selected.copied_text, "alpha\nbravo");
    });
}

#[test]
fn reverse_drag_between_labels_keeps_transcript_order() {
    let messages = vec!["alpha".to_owned(), "bravo".to_owned()];
    let (transcript, observed, data) = create_transcript(messages);

    Harness::create_simple(data, transcript, |harness| {
        harness.send_initial_events();
        harness.just_layout();
        let geometry = snapshot(harness, &observed);

        harness.event(Event::MouseDown(mouse_event(
            text_point(&geometry, 1, 300.0),
            MouseButton::Left,
            Modifiers::default(),
        )));
        harness.event(Event::MouseMove(move_mouse(text_point(&geometry, 0, 0.0))));
        harness.event(Event::MouseUp(mouse_event(
            text_point(&geometry, 0, 0.0),
            MouseButton::Left,
            Modifiers::default(),
        )));

        let selected = snapshot(harness, &observed);
        assert_eq!(selected.fragments[0].as_deref(), Some("alpha"));
        assert_eq!(selected.fragments[1].as_deref(), Some("bravo"));
        assert_eq!(selected.copied_text, "alpha\nbravo");
    });
}

#[test]
fn shift_click_extends_across_labels() {
    let messages = vec!["alpha".to_owned(), "bravo".to_owned()];
    let (transcript, observed, data) = create_transcript(messages);

    Harness::create_simple(data, transcript, |harness| {
        harness.send_initial_events();
        harness.just_layout();
        let geometry = snapshot(harness, &observed);
        let anchor = text_point(&geometry, 0, 0.0);

        harness.event(Event::MouseDown(mouse_event(
            anchor,
            MouseButton::Left,
            Modifiers::default(),
        )));
        harness.event(Event::MouseUp(mouse_event(
            anchor,
            MouseButton::Left,
            Modifiers::default(),
        )));
        observed.borrow_mut().changes.clear();

        let active = text_point(&geometry, 1, 300.0);
        harness.event(Event::MouseDown(mouse_event(
            active,
            MouseButton::Left,
            Modifiers::SHIFT,
        )));
        harness.event(Event::MouseUp(mouse_event(
            active,
            MouseButton::Left,
            Modifiers::default(),
        )));

        let selected = snapshot(harness, &observed);
        assert_eq!(selected.copied_text, "alpha\nbravo");
    });
}
