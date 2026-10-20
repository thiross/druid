# TUI and selectable text

Druid's terminal backend uses the `tui` feature. Build an application against
the terminal backend with:

```text
cargo run --no-default-features --features tui
```

The native and web backends continue to use their normal Piet renderers.

## Layout coordinates and terminal cells

Terminal glyphs must start on a cell boundary. Piet therefore exposes:

```rust
fn layout_origin(&self, requested: Point) -> Point
```

For native and web backends this method returns `requested` unchanged. The TUI
backend snaps the requested origin to its terminal cell grid, using the same
calculation as `draw_text`.

Hit testing and `rects_for_range` still return layout coordinates relative to
the requested origin. To align them with the visible text, use:

```rust
let offset = layout.layout_origin(requested) - requested;
let layout_point = pointer - offset;
let hit = layout.hit_test_point(layout_point);
for rect in layout.rects_for_range(selection) {
    context.fill(rect + offset, &selection_color);
}
```

Druid's `WidgetPod` uses the same cell-aligned origin for painting and pointer
event routing. A widget laid out at a half-cell position can therefore receive
clicks on the cells where its text is actually drawn. GUI behavior is unchanged
because this snapping is only compiled with the `tui` feature.

## Selectable read-only text

`SelectableLabel` provides selection and copying for read-only
`TextStorage` data. It supports dragging, shift-click, double-click word
selection, triple-click logical-line selection, `commands::COPY`, and
`commands::SELECT_ALL`.

```rust
use druid::text::RichText;
use druid::widget::SelectableLabel;
use druid::{ArcStr, Color};

let text = RichText::new(ArcStr::from("Read-only text can be selected and copied."));
let label = SelectableLabel::new()
    .with_text_color(Color::rgb8(210, 210, 210))
    .with_on_copy(|copied| {
        println!("copied {} bytes", copied.len());
    });
```

When the label is embedded in a larger application state, expose the text with
a lens:

```rust
let label = SelectableLabel::<RichText>::new().lens(AppState::note);
```

Copying writes to the application clipboard. The widget also emits
`SelectableLabel::COPY` with the copied `ArcStr`, and the optional
`with_on_copy` callback can be used for application-specific status messages.

## Rich text migration from application widgets

Markdown-aware application widgets should render their source into `RichText`
and then use `SelectableLabel<RichText>` for selection and clipboard behavior.
Markdown parsing, LaTeX rendering, and status-bar messages remain application
concerns.

The half-cell padding workaround used by earlier TUI applications is no longer
required for hit testing. Ordinary spacing around the widget can remain, but it
is now visual layout rather than a correctness workaround.
