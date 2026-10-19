// Copyright 2026 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! Terminal-specific painting helpers.

use crate::kurbo::{Point, Rect, Size};
use crate::piet::{Color, RenderContext, TuiCellExt};
use crate::PaintCtx;

/// Snap a point to the terminal cell grid.
pub(crate) fn snap_point(point: Point) -> Point {
    let cell = cell_size();
    Point::new(
        (point.x / cell.width).round() * cell.width,
        (point.y / cell.height).round() * cell.height,
    )
}

fn cell_size() -> Size {
    crate::shell::tui_cell_size()
}

/// Round a size up to the next whole number of terminal cells.
pub(crate) fn snap_size_up(size: Size) -> Size {
    let cell = cell_size();
    Size::new(
        (size.width / cell.width).ceil() * cell.width,
        (size.height / cell.height).ceil() * cell.height,
    )
}

/// Paint a one-cell border using box-drawing glyphs.
///
/// A vector stroke narrower than one terminal cell would otherwise paint the
/// entire cell background, which makes borders look extremely thick. Cell
/// coordinates ignore the affine transform, so convert the rectangle into the
/// window's terminal grid first. `background_color` is applied to every border
/// cell.
pub(crate) fn paint_cell_border(
    ctx: &mut PaintCtx<'_, '_, '_>,
    rect: Rect,
    color: Color,
    background_color: Color,
    rounded: bool,
) {
    let transform = ctx.current_transform();
    let top_left = transform * Point::new(rect.x0, rect.y0);
    let bottom_right = transform * Point::new(rect.x1, rect.y1);
    if !top_left.x.is_finite()
        || !top_left.y.is_finite()
        || !bottom_right.x.is_finite()
        || !bottom_right.y.is_finite()
    {
        return;
    }

    let cell_width = ctx.render_ctx.config().cell_width();
    let cell_height = ctx.render_ctx.config().cell_height();
    let area = ctx.render_ctx.area();
    if cell_width <= 0.0 || cell_height <= 0.0 {
        return;
    }

    let left = (top_left.x / cell_width).floor() as i32 - i32::from(area.x);
    let right = (bottom_right.x / cell_width).ceil() as i32 - 1 - i32::from(area.x);
    let top = (top_left.y / cell_height).floor() as i32 - i32::from(area.y);
    let bottom = (bottom_right.y / cell_height).ceil() as i32 - 1 - i32::from(area.y);

    if left < 0 || top < 0 || right < left || bottom < top {
        return;
    }

    let left = left as u16;
    let right = right as u16;
    let top = top as u16;
    let bottom = bottom as u16;
    let (top_left_glyph, top_right_glyph, bottom_left_glyph, bottom_right_glyph) = if rounded {
        ("╭", "╮", "╰", "╯")
    } else {
        ("┌", "┐", "└", "┘")
    };

    if top == bottom {
        for col in left..=right {
            ctx.render_ctx
                .set_cell(col, top, "─", color, background_color);
        }
        return;
    }

    if left == right {
        for row in top..=bottom {
            ctx.render_ctx
                .set_cell(left, row, "│", color, background_color);
        }
        return;
    }

    ctx.render_ctx
        .set_cell(left, top, top_left_glyph, color, background_color);
    ctx.render_ctx
        .set_cell(right, top, top_right_glyph, color, background_color);
    ctx.render_ctx
        .set_cell(left, bottom, bottom_left_glyph, color, background_color);
    ctx.render_ctx
        .set_cell(right, bottom, bottom_right_glyph, color, background_color);

    for col in left + 1..right {
        ctx.render_ctx
            .set_cell(col, top, "─", color, background_color);
        ctx.render_ctx
            .set_cell(col, bottom, "─", color, background_color);
    }
    for row in top + 1..bottom {
        ctx.render_ctx
            .set_cell(left, row, "│", color, background_color);
        ctx.render_ctx
            .set_cell(right, row, "│", color, background_color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snaps_points_to_cell_grid() {
        let cell = cell_size();
        let point = Point::new(cell.width * 2.5, cell.height * 3.25);
        assert_eq!(
            snap_point(point),
            Point::new(cell.width * 3.0, cell.height * 3.0)
        );
    }

    #[test]
    fn snaps_sizes_up_to_cell_grid() {
        let cell = cell_size();
        let size = Size::new(cell.width * 2.5, cell.height * 1.25);
        assert_eq!(
            snap_size_up(size),
            Size::new(cell.width * 3.0, cell.height * 2.0)
        );
    }
}
