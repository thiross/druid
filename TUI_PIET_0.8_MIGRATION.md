# Druid Piet 0.8 and TUI Backend Migration Handoff

Status: handoff for Linux migration and validation
Branch: `tui`
Base: `master` at `0f8b1195`
Date: 2026-10-18

## 1. Goal

Move Druid from the currently pinned Piet backend to the Piet 0.8 line, so that:

- the native Linux backend can be validated against Piet 0.8; and
- the new `piet-common/tui` backend can eventually be selected by `druid-shell`.

The TUI backend itself is not implemented on this branch yet. This document records the dependency decision, the expected Linux work, and the known follow-up items for the TUI shell integration.

## 2. Upstream Piet state

The local/forked Piet repository is on branch `tui`.

Relevant upstream commit:

```text
0b210da Add TUI backend to piet-common
517db1d Add terminal cell drawing extension
```

The `tui` branch currently provides:

- `piet` 0.8.0
- `piet-tui` 0.8.0
- `piet-common` 0.8.0 with a `tui` feature
- `piet-common/tui` exposes:
  - `Piet`
  - `Brush`
  - `PietText`
  - `PietTextLayout`
  - `PietTextLayoutBuilder`
  - `PietImage`
  - `Device`
  - `BitmapTarget`
  - `TuiConfig`
  - `TuiRenderContext`
  - `ratatui_core`
- `piet-common` native dependencies are optional and enabled by `native`
- `piet-common/tui` compiles without Cairo/CoreGraphics/Direct2D/Web dependencies when default features are disabled

Useful dependency form:

```toml
piet-common = {
    git = "https://github.com/thiross/piet.git",
    branch = "tui",
    default-features = false,
}
```

For local development, the equivalent path is:

```toml
piet-common = {
    path = "../piet/piet-common",
    default-features = false,
}
```

Always enable either `native` or `tui` explicitly when default features are disabled.

## 3. Current Druid dependency situation

Current Druid manifests are still on the older Piet line:

- `druid-shell/Cargo.toml`:
  - `piet-common = "=0.7.0-cairo18"`
  - `cairo-rs = "0.18.5"`
  - `gtk = "0.18.2"`
- `druid/Cargo.toml`:
  - dev-dependency `piet-common = "=0.7.0-cairo18"`

Piet 0.8 changes the important native dependency versions:

- `piet-cairo` uses `cairo-rs = 0.22.0`
- `piet-cairo` uses `pango = 0.22.4`
- `piet-cairo` uses `pangocairo = 0.22.0`
- `piet` uses `kurbo = 0.13.0`
- Piet 0.8 requires Rust 1.92 or newer
- Piet workspace uses edition 2024

This means a direct global switch to Piet 0.8 is not just a version edit. The Linux GTK/Cairo stack must be aligned with the Cairo version expected by `piet-cairo` 0.8.

## 4. Why Linux must be migrated first

The GTK backend in `druid-shell` passes a Cairo context into Piet:

- `druid-shell/src/backend/gtk/window.rs` imports `cairo::Surface`
- `druid-shell/src/backend/gtk/window.rs` creates a Cairo context and calls `Piet::new(&surface_context)`

With the current manifest, GTK 0.18 brings Cairo 0.18 types, while `piet-cairo` 0.8 brings Cairo 0.22 types. Two different versions of the `cairo-rs` crate are distinct Rust types, so the GTK path will not type-check until the Cairo/GTK stack is aligned.

The Linux migration should therefore:

1. update `piet-common` to the 0.8 fork;
2. update the GTK/Cairo stack together;
3. fix any resulting GTK/X11/Wayland type changes; and
4. verify the native backend before starting TUI shell work.

## 5. Recommended feature model

Piet 0.8 should remain backend-selectable. For `druid-shell`, a feature model close to this is recommended:

```toml
[features]
default = ["gtk"]

native = ["dep:piet-common"]
tui = ["dep:piet-common", "dep:ratatui"]

gtk = ["native", "gtk-rs", ...]
x11 = ["native", ...]
wayland = ["native", ...]
```

`piet-common` itself defaults to `native`. When using it from a TUI build, `default-features = false` is mandatory:

```toml
piet-common = {
    git = "https://github.com/thiross/piet.git",
    branch = "tui",
    default-features = false,
    features = ["tui"],
}
```

Do not use feature conditions in Cargo target-dependency tables. Cargo warns that `feature = ...` inside `[target.'cfg(...)'.dependencies]` is not supported for selecting dependencies:

```toml
# Do not use this pattern:
[target.'cfg(not(feature = "tui"))'.dependencies]
```

Feature selection belongs in Rust `cfg` attributes and in the `[features]` section. Native target dependencies should be optional and enabled through `native`, `gtk`, `x11`, or `wayland`.

In `druid-shell/src/backend/mod.rs`, select TUI first and exclude it from every native branch:

```rust
#[cfg(feature = "tui")]
mod tui;
#[cfg(feature = "tui")]
pub use tui::*;

#[cfg(all(not(feature = "tui"), target_os = "macos"))]
mod mac;
#[cfg(all(not(feature = "tui"), target_os = "macos"))]
pub use mac::*;

#[cfg(all(not(feature = "tui"), target_os = "windows"))]
mod windows;
#[cfg(all(not(feature = "tui"), target_os = "windows"))]
pub use windows::*;
```

Apply the same `not(feature = "tui")` guard to Linux x11/wayland/gtk and wasm branches.

A hard mutual-exclusion guard is also recommended in `druid-shell`:

```rust
#[cfg(all(
    feature = "tui",
    any(feature = "gtk", feature = "x11", feature = "wayland")
))]
compile_error!("`tui` is mutually exclusive with native Druid backends");
```

## 6. Linux migration tasks

### 6.1 Manifest updates

- Change `druid-shell` `piet-common` to the Piet 0.8 fork.
- Change `druid` dev-dependency `piet-common` to the same source/version.
- Update `cairo-rs` and the GTK stack to versions compatible with `piet-cairo` 0.8.
- Check whether `cairo-rs/xcb` is still available and required by the x11 backend.
- Decide whether native platform dependencies should be optional immediately or in a follow-up feature-isolation change.
- Regenerate `Cargo.lock`.

### 6.2 Native compile fixes

Check these areas first:

- `druid-shell/src/backend/gtk/window.rs`
  - Cairo `Surface`/`Context` ownership and borrow types
  - `Piet::new(&surface_context)`
  - Cairo region and drawing-area code
- `druid-shell/src/backend/x11`
  - Cairo surface/context construction
  - any direct `cairo-rs` use
- `druid-shell/src/backend/wayland`
  - Cairo and Piet context construction
- `druid-shell/src/window.rs`
  - `WinHandler::paint` currently names `piet_common::Piet`; this can remain if a single Piet source is used, but using `crate::piet::Piet` is clearer and avoids future feature-selection mistakes.
- `druid/src/text`, `druid/src/widget/image.rs`, and tests
  - verify kurbo 0.13 type compatibility
  - verify `PietTextLayout`/`PietImage` associated type usage

### 6.3 Dependency duplicate checks

Run:

```bash
cargo tree -d
cargo tree -p druid-shell -i cairo-rs
cargo tree -p druid-shell -i piet-common
cargo tree -p druid-shell -i kurbo
```

Expected result:

- one active Cairo version in the native build
- one active kurbo version in the native build
- no `piet-common` 0.7 and 0.8 mixed in the same feature build

If both versions appear, do not proceed to TUI shell work until the graph is understood.

## 7. Validation commands for Linux

Start with the native backend:

```bash
cargo check -p druid-shell
cargo check -p druid
cargo test -p druid-shell
cargo test -p druid
```

Then run representative examples:

```bash
cargo run -p druid --example hello
cargo run -p druid --example textbox
cargo run -p druid --example scroll
cargo run -p druid --example list --features im
cargo run -p druid --example image --features image,png
cargo run -p druid --example svg --features svg
```

At minimum, verify:

- window creation and event loop
- text rendering and text input
- mouse/keyboard/timer/invalidation behavior
- image and SVG rendering
- GTK clipboard and menu behavior
- clean exit and window destruction

## 8. Known TUI integration follow-ups

These are not Linux native-migration blockers, but they must be handled before the TUI backend is usable.

### 8.1 Text factory construction

`piet-common` maps `PietText` to `TuiText`, but `TuiText::new` is still `pub(crate)` in `piet-tui/src/text.rs`.

Druid's `WindowHandle::text()` needs to create a text factory independently of a render context. Add one of:

```rust
impl TuiText {
    pub fn from_config(config: TuiConfig) -> Self;
}
```

and preferably:

```rust
impl Device {
    pub fn text(&self) -> PietText;
}
```

The text factory and `TuiRenderContext` must use the same `TuiConfig`.

### 8.2 BitmapTarget dimensions

`piet-common` currently documents that `TUI Device::bitmap_target(width, height, pix_scale)` interprets width/height as terminal cells.

Druid's test harness passes display-point dimensions, for example `400x400`. With an `8x16` cell size this should be approximately `50x25` cells, not `400x400` cells.

Add a helper such as:

```rust
pub fn bitmap_target_for_size(&mut self, size: Size) -> Result<BitmapTarget<'_>, Error>;
```

or adapt `druid/src/tests/harness.rs` under `cfg(feature = "tui")`.

### 8.3 Pixel export tests

`piet-common/tui` returns `Error::NotSupported` from:

- `BitmapTarget::to_image_buf`
- `BitmapTarget::copy_raw_pixels`
- `BitmapTarget::save_to_file`

Druid tests that call `TargetGuard::into_raw` or `into_png` must be gated with:

```rust
#[cfg(not(feature = "tui"))]
```

TUI rendering tests should inspect a ratatui `Buffer` or a cell snapshot instead of RGBA pixels.

### 8.4 TUI shell backend

The shell backend still needs:

- terminal initialization/restoration
- alternate screen and raw mode
- crossterm event loop
- `prepare_paint`/`paint` integration
- size and mouse coordinate mapping
- timer and idle queues
- keyboard mapping through `simulate_input`
- bracketed paste
- OSC52 clipboard output and documented clipboard-read limitation
- one-window restriction for the terminal surface

Recommended TUI cell configuration for Druid:

```rust
TuiConfig::with_cell_size(Size::new(8.0, 16.0))
```

## 9. Acceptance criteria

Native Linux migration is complete when:

- `cargo check -p druid-shell` and `cargo check -p druid` pass
- `cargo test -p druid-shell` and `cargo test -p druid` pass
- representative examples run successfully
- `cargo tree` shows no mixed Cairo versions
- no `piet-common` 0.7 dependency remains
- Rust version is at least 1.92

TUI shell work can then start with:

- `druid-shell` `tui` feature and `not(feature = "tui")` backend guards
- `piet-common/tui` with `default-features = false`
- `piet-common` text factory and test-target fixes listed above
- a dedicated TUI smoke example before broader widget compatibility testing

## 10. Decisions and remaining questions

Already decided:

- Move the whole Druid workspace to Piet 0.8; do not keep a long-term native 0.7 / TUI 0.8 split.
- Select the TUI backend through a `druid-shell` `tui` feature, not by exposing `piet-tui` directly to Druid.
- Gate every native backend with `not(feature = "tui")` and reject incompatible feature combinations with `compile_error!`.
- Use `piet-common` with `default-features = false` plus `features = ["tui"]` for TUI builds.
- Complete and validate the native Linux migration on this branch before starting TUI shell implementation.

Remaining questions:

- Which GTK release and API migration path will align cleanly with `piet-cairo` 0.8's Cairo 0.22 stack.
- Whether TUI supports only one terminal window or implements virtual window switching.
- Whether menus/file dialogs remain explicit no-ops or receive a TUI-specific implementation later.
