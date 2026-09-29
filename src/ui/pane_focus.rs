use ratatui::{layout::Rect, style::Modifier};

use super::panes::{is_pane_border_symbol, line_touches_pane};
use crate::protocol::{
    CellData, ClientShellPaneFocusStyle, FrameData, PaneSurfacePane, SurfaceRect,
};

/// Redraws the focus-dependent chrome of a pane surface as the endpoint draws it when
/// `panes[focused]` has focus: border lines, border titles and scrollbars. Terminal cells and
/// the cursor are left alone.
pub(crate) fn restyle_pane_surface_focus(
    frame: &mut FrameData,
    panes: &[PaneSurfacePane],
    focused: usize,
    style: ClientShellPaneFocusStyle,
) {
    let colors = style.colors();
    let accent = crate::protocol::color_to_u32(colors.accent);
    let overlay0 = crate::protocol::color_to_u32(colors.overlay0);
    let focused_rect = rect(panes[focused].rect);
    let is_chrome = |x: u16, y: u16| {
        panes.iter().all(|pane| {
            !contains(pane.inner_rect, x, y)
                && pane
                    .scrollbar_rect
                    .is_none_or(|scrollbar| !contains(scrollbar, x, y))
        })
    };
    let is_border_color = |cell: &CellData| cell.fg == accent || cell.fg == overlay0;

    // Pane rects do not overlap and every border cell lies on some pane's perimeter.
    for (index, pane) in panes.iter().enumerate() {
        let pane_focused = index == focused;
        let title = title_span(frame, pane, &is_chrome, &is_border_color);
        for (x, y) in perimeter(rect(pane.rect)) {
            if !is_chrome(x, y) {
                continue;
            }
            let Some(cell) = cell_mut(frame, x, y) else {
                continue;
            };
            if !is_border_color(cell) {
                continue;
            }
            if y == pane.rect.y && title.contains(&x) {
                cell.fg = crate::protocol::color_to_u32(colors.border(pane_focused));
                let modifier = Modifier::from_bits_retain(cell.modifier);
                let modifier = if pane_focused {
                    modifier | Modifier::BOLD
                } else {
                    modifier - Modifier::BOLD
                };
                cell.modifier = modifier.bits();
            } else if is_pane_border_symbol(&cell.symbol) {
                let touches = line_touches_pane(x, y, focused_rect, style.pane_gaps);
                cell.fg = crate::protocol::color_to_u32(colors.border(touches));
            }
        }
        restyle_scrollbar(frame, pane, colors, pane_focused);
    }
}

/// The columns of the border title drawn on `pane`'s top edge. Titles are padded with a space
/// on each side and drawn over the border line from `x + 1`, so the title ends where the line
/// resumes.
fn title_span(
    frame: &FrameData,
    pane: &PaneSurfacePane,
    is_chrome: &impl Fn(u16, u16) -> bool,
    is_border_color: &impl Fn(&CellData) -> bool,
) -> std::ops::Range<u16> {
    let y = pane.rect.y;
    let start = pane.rect.x.saturating_add(1);
    let end = pane
        .rect
        .x
        .saturating_add(pane.rect.width)
        .saturating_sub(1);
    let starts_title = pane.rect.width > 4
        && is_chrome(start, y)
        && cell(frame, start, y).is_some_and(|cell| cell.symbol == " " && is_border_color(cell));
    if !starts_title {
        return start..start;
    }
    let title_end = (start..end)
        .find(|&x| cell(frame, x, y).is_none_or(|cell| is_pane_border_symbol(&cell.symbol)))
        .unwrap_or(end);
    start..title_end
}

fn restyle_scrollbar(
    frame: &mut FrameData,
    pane: &PaneSurfacePane,
    colors: super::PaneFocusColors,
    focused: bool,
) {
    let (Some(track), Some(scroll)) = (pane.scrollbar_rect, pane.scroll) else {
        return;
    };
    let metrics = crate::pane::ScrollMetrics {
        offset_from_bottom: usize::try_from(scroll.offset_from_bottom).unwrap_or(usize::MAX),
        max_offset_from_bottom: usize::try_from(scroll.max_offset_from_bottom)
            .unwrap_or(usize::MAX),
        viewport_rows: usize::try_from(scroll.viewport_rows).unwrap_or(usize::MAX),
    };
    let area = Rect::new(0, 0, 1, track.height);
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    let (track_color, thumb_color, thumb_symbol) = colors.scrollbar(focused);
    super::render_scrollbar_buffer(
        &mut buffer,
        metrics,
        area,
        track_color,
        thumb_color,
        thumb_symbol,
    );
    for (row, rendered) in (0..track.height).zip(&buffer.content) {
        if let Some(cell) = cell_mut(frame, track.x, track.y.saturating_add(row)) {
            *cell = CellData::from_ratatui_cell(rendered);
        }
    }
}

fn perimeter(rect: Rect) -> impl Iterator<Item = (u16, u16)> {
    let (xs, ys) = if rect.is_empty() {
        (0..0, 0..0)
    } else {
        (rect.x..rect.right(), rect.y..rect.bottom())
    };
    let right = rect.right().saturating_sub(1);
    let bottom = rect.bottom().saturating_sub(1);
    let rows = xs.flat_map(move |x| [(x, rect.y), (x, bottom)]);
    let columns = ys.flat_map(move |y| [(rect.x, y), (right, y)]);
    rows.chain(columns)
}

fn rect(rect: SurfaceRect) -> Rect {
    Rect::new(rect.x, rect.y, rect.width, rect.height)
}

fn contains(rect: SurfaceRect, x: u16, y: u16) -> bool {
    self::rect(rect).contains(ratatui::layout::Position { x, y })
}

fn cell(frame: &FrameData, x: u16, y: u16) -> Option<&CellData> {
    (x < frame.width && y < frame.height)
        .then(|| {
            frame
                .cells
                .get(usize::from(y) * usize::from(frame.width) + usize::from(x))
        })
        .flatten()
}

fn cell_mut(frame: &mut FrameData, x: u16, y: u16) -> Option<&mut CellData> {
    let width = frame.width;
    (x < width && y < frame.height)
        .then(|| {
            frame
                .cells
                .get_mut(usize::from(y) * usize::from(width) + usize::from(x))
        })
        .flatten()
}
