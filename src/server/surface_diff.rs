//! Scroll-aware delivery of a complete pane surface render.
//!
//! A client whose render slot filled up recovers through the complete renderer, which only
//! knows two complete surfaces. This module recovers the row patch between them, so a
//! scrolled pane can still travel as the compact scroll message.

use crate::protocol::{
    CellData, PaneSurfaceFrame, PaneSurfacePatch, PaneSurfacePatchRow, ServerMessage, SurfaceRect,
};

/// A pane region of the surface a row patch may carry: terminal cells or a scrollbar column.
struct Region {
    rect: SurfaceRect,
    pane: usize,
    scrollbar: bool,
}

/// Encodes `next` against the committed `last` as a scroll message, returning it with the
/// patch it encodes. `None` means ordinary delta or full encoding applies.
pub(crate) fn scroll_message(
    last: &PaneSurfaceFrame,
    next: &PaneSurfaceFrame,
    surface_revision: u64,
) -> Option<(ServerMessage, Box<PaneSurfacePatch>)> {
    let mut patch = pane_patch(last, next)?;
    patch.surface_revision = surface_revision;
    let message = crate::protocol::surface_scroll::message(last, &patch)?;
    Some((message, Box::new(patch)))
}

fn has_graphics(surface: &PaneSurfaceFrame) -> bool {
    let graphics = &surface.graphics;
    !graphics.assets.is_empty()
        || !graphics.placements.is_empty()
        || !graphics.retained_assets.is_empty()
}

fn row_of(cells: &[CellData], width: usize, y: usize) -> &[CellData] {
    &cells[y * width..(y + 1) * width]
}

fn fits_frame(rect: SurfaceRect, frame_width: u16, frame_height: u16) -> bool {
    rect.x
        .checked_add(rect.width)
        .is_some_and(|right| right <= frame_width)
        && rect
            .y
            .checked_add(rect.height)
            .is_some_and(|bottom| bottom <= frame_height)
}

/// The row patch that turns `last` into `next`, or `None` when the two surfaces differ in
/// anything a row patch cannot carry or when scrolling cannot be what changed.
///
/// A client applies a patch without recomposing, so only terminal and scrollbar cells of panes
/// may change, over identical geometry. Applying the result to `last` yields `next`.
fn pane_patch(last: &PaneSurfaceFrame, next: &PaneSurfaceFrame) -> Option<PaneSurfacePatch> {
    let (frame_width, frame_height) = (next.frame.width, next.frame.height);
    let (width, height) = (usize::from(frame_width), usize::from(frame_height));
    // A row patch carries no hyperlink table, and the retained patch path refuses them too.
    if last.boot_id != next.boot_id
        || last.projection_revision != next.projection_revision
        || last.frame.width != frame_width
        || last.frame.height != frame_height
        || last.frame.cells.len() != width * height
        || next.frame.cells.len() != width * height
        || !last.frame.hyperlinks.is_empty()
        || !next.frame.hyperlinks.is_empty()
        || !last.frame.graphics.is_empty()
        || !next.frame.graphics.is_empty()
        || last.popup.is_some()
        || next.popup.is_some()
        || has_graphics(last)
        || has_graphics(next)
        || last.splits != next.splits
        || last.panes.len() != next.panes.len()
    {
        return None;
    }

    let mut regions = Vec::with_capacity(next.panes.len() * 2);
    for (index, (old, new)) in last.panes.iter().zip(&next.panes).enumerate() {
        if old.pane_id != new.pane_id
            || old.rect != new.rect
            || old.inner_rect != new.inner_rect
            || old.focused != new.focused
            || old.pixel_width != new.pixel_width
            || old.pixel_height != new.pixel_height
        {
            return None;
        }
        // The client accepts scrollbar rows against the new rect, else the committed one.
        let scrollbar = new.scrollbar_rect.or(old.scrollbar_rect);
        for (rect, scrollbar) in [(Some(new.inner_rect), false), (scrollbar, true)] {
            let Some(rect) = rect.filter(|rect| rect.width > 0 && rect.height > 0) else {
                continue;
            };
            if !fits_frame(rect, frame_width, frame_height) {
                return None;
            }
            regions.push(Region {
                rect,
                pane: index,
                scrollbar,
            });
        }
    }

    // Scrolling output moves most rows of a pane, while typing moves one. Probing a few rows
    // spares the common edit a second pass over the surface.
    let moving = regions
        .iter()
        .filter(|region| !region.scrollbar)
        .any(|region| {
            let rect = region.rect;
            let (left, right) = (usize::from(rect.x), usize::from(rect.x + rect.width));
            let probes = [rect.height / 4, rect.height / 2, rect.height / 4 * 3];
            probes
                .into_iter()
                .filter(|probe| {
                    let y = usize::from(rect.y + probe);
                    row_of(&last.frame.cells, width, y)[left..right]
                        != row_of(&next.frame.cells, width, y)[left..right]
                })
                .count()
                >= 2
        });
    if !moving {
        return None;
    }

    let mut rows = Vec::new();
    let mut changed = vec![false; next.panes.len()];
    // Start, end, and index into `regions` of the regions that cross the current row.
    let mut crossing: Vec<(usize, usize, usize)> = Vec::new();
    for y in 0..height {
        let (old_row, new_row) = (
            row_of(&last.frame.cells, width, y),
            row_of(&next.frame.cells, width, y),
        );
        crossing.clear();
        crossing.extend(regions.iter().enumerate().filter_map(|(index, region)| {
            let rect = region.rect;
            let inside = y >= usize::from(rect.y) && y < usize::from(rect.y + rect.height);
            inside.then_some((usize::from(rect.x), usize::from(rect.x + rect.width), index))
        }));
        crossing.sort_unstable();
        // Cells between regions are pane chrome, which a patch cannot repaint.
        let mut covered = 0;
        for &(start, end, index) in &crossing {
            if start < covered || old_row[covered..start] != new_row[covered..start] {
                return None;
            }
            covered = end;
            let region = &regions[index];
            if region.scrollbar {
                if old_row[start..end] != new_row[start..end] {
                    rows.push(PaneSurfacePatchRow {
                        x: region.rect.x,
                        y: y as u16,
                        cells: new_row[start..end].to_vec(),
                    });
                    changed[region.pane] = true;
                }
                continue;
            }
            let mut x = start;
            while x < end {
                if old_row[x] == new_row[x] {
                    x += 1;
                    continue;
                }
                let run = x;
                while x < end && old_row[x] != new_row[x] {
                    x += 1;
                }
                // Repaint the next cell too, so a width change leaves no stale glyph half.
                let span_end = (x + 1).min(end);
                rows.push(PaneSurfacePatchRow {
                    x: run as u16,
                    y: y as u16,
                    cells: new_row[run..span_end].to_vec(),
                });
                changed[region.pane] = true;
                x = span_end;
            }
        }
        if old_row[covered..] != new_row[covered..] {
            return None;
        }
    }
    if rows.len() < 2 {
        return None;
    }

    let panes = last
        .panes
        .iter()
        .zip(&next.panes)
        .zip(&changed)
        .filter(|((old, new), &changed)| changed || old != new)
        .map(|((_, new), _)| new.clone())
        .collect();
    Some(PaneSurfacePatch {
        boot_id: next.boot_id.clone(),
        projection_revision: next.projection_revision,
        base_surface_revision: last.surface_revision,
        surface_revision: 0,
        rows,
        panes,
        cursor: next.frame.cursor.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CursorState, FrameData, PaneSurfacePane};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 20;

    fn rect(x: u16, y: u16, width: u16, height: u16) -> SurfaceRect {
        SurfaceRect {
            x,
            y,
            width,
            height,
        }
    }

    fn pane(id: &str, inner: SurfaceRect) -> PaneSurfacePane {
        PaneSurfacePane {
            pane_id: id.into(),
            content_revision: 1,
            rect: rect(inner.x - 1, inner.y - 1, inner.width + 2, inner.height + 2),
            inner_rect: inner,
            scrollbar_rect: None,
            scroll: None,
            focused: true,
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            alternate_screen_active: false,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// One bordered pane whose rows show `first..first + 18`.
    fn surface(first: usize) -> PaneSurfaceFrame {
        let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
        for y in 0..HEIGHT {
            buffer.set_string(0, y, "|", Style::default());
            buffer.set_string(WIDTH - 1, y, "|", Style::default());
        }
        for y in 1..HEIGHT - 1 {
            let n = first + usize::from(y) - 1;
            buffer.set_string(
                1,
                y,
                format!("line {n:>4}: some build output"),
                Style::default(),
            );
        }
        let cursor = CursorState {
            x: 3,
            y: 5,
            visible: true,
            shape: 0,
        };
        PaneSurfaceFrame {
            boot_id: "boot".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame: FrameData::from_ratatui_buffer(&buffer, Some(cursor)),
            panes: vec![pane("p1", rect(1, 1, WIDTH - 2, HEIGHT - 2))],
            splits: Vec::new(),
            popup: None,
            graphics: Default::default(),
        }
    }

    fn scrolled_next(last: &PaneSurfaceFrame, lines: usize) -> PaneSurfaceFrame {
        let mut next = surface(lines);
        next.surface_revision = last.surface_revision + 1;
        next.panes[0].content_revision += 1;
        next
    }

    #[test]
    fn a_scrolled_pane_becomes_a_patch_that_rebuilds_the_next_surface() {
        let last = surface(0);
        let next = scrolled_next(&last, 3);
        let mut patch = pane_patch(&last, &next).expect("scroll patch");
        patch.surface_revision = next.surface_revision;
        assert_eq!(patch.base_surface_revision, last.surface_revision);
        assert_eq!(patch.panes.len(), 1);
        let mut rebuilt = last.clone();
        crate::server::render_stream::apply_pane_surface_patch(&mut rebuilt, &patch);
        assert_eq!(rebuilt, next);

        let (message, _) = scroll_message(&last, &next, next.surface_revision).expect("scroll");
        assert!(matches!(
            message,
            ServerMessage::EndpointControl { kind, .. }
                if kind == crate::protocol::surface_scroll::MESSAGE_KIND
        ));
    }

    #[test]
    fn edits_that_do_not_move_rows_are_left_to_the_delta() {
        let last = surface(0);
        let mut next = last.clone();
        next.surface_revision += 1;
        next.frame.cells[usize::from(WIDTH) * 3 + 5].symbol = "x".into();
        assert!(pane_patch(&last, &next).is_none());
        // Every row changes, but nothing moves, so the shift cannot pay for itself.
        let mut rewritten = last.clone();
        for y in 1..usize::from(HEIGHT) - 1 {
            for x in 1..usize::from(WIDTH) - 1 {
                let letter = char::from(b'a' + ((y * 5 + x * 3) % 26) as u8);
                rewritten.frame.cells[y * usize::from(WIDTH) + x].symbol = letter.to_string();
            }
        }
        assert!(pane_patch(&last, &rewritten).is_some());
        assert!(scroll_message(&last, &rewritten, 2).is_none());
    }

    #[test]
    fn changes_a_patch_cannot_carry_are_refused() {
        let last = surface(0);
        let scrolled = scrolled_next(&last, 3);
        assert!(pane_patch(&last, &scrolled).is_some());
        let refused = |edit: &dyn Fn(&mut PaneSurfaceFrame)| {
            let mut next = scrolled.clone();
            edit(&mut next);
            pane_patch(&last, &next).is_none()
        };
        // Chrome outside the pane.
        assert!(refused(
            &|next| next.frame.cells[usize::from(WIDTH) * 4].symbol = "#".into()
        ));
        assert!(refused(&|next| next.frame.cells[0].symbol = "#".into()));
        // Layout and projection.
        assert!(refused(&|next| next.panes[0].focused = false));
        assert!(refused(&|next| next.panes[0].inner_rect.width -= 1));
        assert!(refused(&|next| next.panes[0].pane_id = "other".into()));
        assert!(refused(&|next| next
            .panes
            .push(pane("p2", rect(1, 1, 1, 1)))));
        assert!(refused(&|next| next.projection_revision += 1));
        assert!(refused(&|next| next.boot_id = "other".into()));
        assert!(refused(&|next| next.splits.push(
            crate::protocol::PaneSurfaceSplit {
                direction: crate::protocol::PaneSurfaceSplitDirection::Vertical,
                pos: 3,
                area: rect(0, 0, 4, 4),
                hit_rect: rect(0, 0, 4, 4),
                path: Vec::new(),
            }
        )));
        // Overlays and graphics.
        assert!(refused(&|next| next
            .frame
            .hyperlinks
            .push("https://example.com".into())));
        assert!(refused(&|next| next.frame.graphics.push(1)));
        assert!(refused(&|next| next.graphics.retained_assets.push(
            crate::protocol::SurfaceGraphicsAssetKey {
                source: crate::protocol::SurfaceGraphicsSource::PaneLayer {
                    pane_id: "p1".into(),
                    layer_id: "image".into(),
                },
                image_width: 1,
                image_height: 1,
                format: crate::protocol::SurfaceGraphicsFormat::Rgba,
                data_len: 4,
                data_fingerprint: 1,
            }
        )));
        // A resize.
        assert!(refused(&|next| {
            next.frame.width += 1;
            next.frame
                .cells
                .truncate(usize::from(WIDTH) * usize::from(HEIGHT));
        }));
    }

    #[test]
    fn a_popup_on_either_surface_is_refused() {
        let popup = Box::new(crate::protocol::ClientShellPopupSurface {
            terminal_id: "popup".into(),
            title: "popup".into(),
            width: None,
            height: None,
            frame: FrameData::from_ratatui_buffer(&Buffer::empty(Rect::new(0, 0, 4, 4)), None),
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            pixel_width: 0,
            pixel_height: 0,
        });
        let last = surface(0);
        let mut next = scrolled_next(&last, 3);
        next.popup = Some(popup.clone());
        assert!(pane_patch(&last, &next).is_none());
        let mut with_popup = last.clone();
        with_popup.popup = Some(popup);
        assert!(pane_patch(&with_popup, &scrolled_next(&with_popup, 3)).is_none());
    }

    mod complete_render {
        use super::*;
        use crate::protocol::surface_reuse::Decoder;
        use crate::protocol::RenderEncoding;
        use crate::server::render_stream::{ClientRenderState, PreparedRender};

        fn is_kind(prepared: &PreparedRender, expected: &str) -> bool {
            matches!(
                prepared.message(),
                ServerMessage::EndpointControl { kind, .. } if kind == expected
            )
        }

        fn is_scroll(prepared: &PreparedRender) -> bool {
            is_kind(prepared, crate::protocol::surface_scroll::MESSAGE_KIND)
        }

        /// A client holding revision 1 of `surface(0)`.
        fn connected(delta: bool, scroll: bool) -> (ClientRenderState, Decoder, PaneSurfaceFrame) {
            let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
            state.enable_surface_delta(delta);
            state.enable_surface_scroll(scroll);
            let mut decoder = Decoder::new(delta, scroll);
            let initial = state.prepare_pane_surface(surface(0)).expect("initial");
            let ServerMessage::PaneSurface(held) =
                decoder.decode(initial.message().clone()).expect("initial")
            else {
                panic!("full surface");
            };
            state.commit_sent_frame(initial);
            (state, decoder, held)
        }

        #[test]
        fn a_scrolled_surface_commits_exactly_what_the_client_holds() {
            for delta in [false, true] {
                let (mut state, mut decoder, mut held) = connected(delta, true);
                for (step, first) in [3, 5, 9].into_iter().enumerate() {
                    let next = surface(first);
                    let prepared = state.prepare_pane_surface(next.clone()).expect("render");
                    assert!(is_scroll(&prepared), "step {step}");
                    let message = prepared.message().clone();
                    let ServerMessage::PaneSurfacePatch(patch) =
                        decoder.decode(message).expect("scroll decode")
                    else {
                        panic!("expanded patch");
                    };
                    crate::server::render_stream::apply_pane_surface_patch(&mut held, &patch);
                    state.commit_sent_frame(prepared);

                    let committed = state.last_pane_surface().expect("committed");
                    let mut expected = next;
                    expected.surface_revision = committed.surface_revision;
                    assert_eq!(committed, &expected);
                    assert_eq!(held, expected, "the client rebuilds the new surface");
                    assert_eq!(committed.surface_revision, step as u64 + 2);
                }
            }
        }

        #[test]
        fn scrolling_is_only_used_when_negotiated_and_nothing_else_asks_for_a_full_surface() {
            let scrolled = surface(3);
            let (mut state, ..) = connected(true, false);
            let prepared = state.prepare_pane_surface(scrolled.clone()).unwrap();
            assert!(!is_scroll(&prepared));

            let (mut state, ..) = connected(true, true);
            let prepared = state
                .prepare_pane_surface_with_file(scrolled.clone(), None, true)
                .unwrap();
            assert!(!is_scroll(&prepared));

            state.request_recompute();
            let prepared = state.prepare_pane_surface(scrolled.clone()).unwrap();
            assert!(!is_scroll(&prepared));

            let (mut state, ..) = connected(true, true);
            state.request_repaint();
            let prepared = state.prepare_pane_surface(scrolled).unwrap();
            assert!(matches!(prepared.message(), ServerMessage::PaneSurface(_)));
        }

        #[test]
        fn non_scroll_and_structural_changes_keep_the_delta_and_full_encodings() {
            let (mut state, ..) = connected(true, true);
            let mut edited = surface(0);
            edited.frame.cells[usize::from(WIDTH) * 3 + 5].symbol = "x".into();
            let prepared = state.prepare_pane_surface(edited).unwrap();
            assert!(is_kind(
                &prepared,
                crate::protocol::surface_delta::MESSAGE_KIND
            ));

            let scrolled = surface(3);
            let mut structural: Vec<(&str, PaneSurfaceFrame)> = Vec::new();
            let mut resized = scrolled.clone();
            resized.frame.width += 1;
            resized.frame.cells =
                vec![scrolled.frame.cells[0].clone(); usize::from(WIDTH + 1) * usize::from(HEIGHT)];
            structural.push(("resize", resized));
            let mut popup = scrolled.clone();
            popup.popup = Some(Box::new(crate::protocol::ClientShellPopupSurface {
                terminal_id: "popup".into(),
                title: "popup".into(),
                width: None,
                height: None,
                frame: FrameData::from_ratatui_buffer(&Buffer::empty(Rect::new(0, 0, 4, 4)), None),
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                pixel_width: 0,
                pixel_height: 0,
            }));
            structural.push(("popup", popup));
            let mut chrome = scrolled.clone();
            chrome.frame.cells[0].symbol = "#".into();
            structural.push(("chrome", chrome));
            let mut graphics = scrolled.clone();
            graphics.frame.graphics = vec![1];
            structural.push(("graphics", graphics));
            for (name, next) in structural {
                let (mut state, ..) = connected(true, true);
                let prepared = state.prepare_pane_surface(next).unwrap();
                assert!(!is_scroll(&prepared), "{name}");
                assert!(
                    matches!(prepared, PreparedRender::Semantic { .. }),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn scrollbar_and_metadata_changes_ride_along_with_the_scroll() {
        let mut last = surface(0);
        last.panes[0].inner_rect.width -= 1;
        last.panes[0].scrollbar_rect = Some(rect(WIDTH - 2, 1, 1, HEIGHT - 2));
        last.frame.cells[usize::from(WIDTH) * 2 + usize::from(WIDTH) - 2].symbol = "#".into();
        let mut next = scrolled_next(&last, 3);
        next.panes[0].inner_rect.width -= 1;
        next.panes[0].scrollbar_rect = Some(rect(WIDTH - 2, 1, 1, HEIGHT - 2));
        next.panes[0].scroll = Some(crate::protocol::PaneSurfaceScrollMetrics {
            offset_from_bottom: 3,
            max_offset_from_bottom: 30,
            viewport_rows: 18,
        });
        next.panes[0].mouse_reporting = true;
        let mut patch = pane_patch(&last, &next).expect("scroll patch with scrollbar");
        patch.surface_revision = next.surface_revision;
        let mut rebuilt = last.clone();
        crate::server::render_stream::apply_pane_surface_patch(&mut rebuilt, &patch);
        assert_eq!(rebuilt, next);
        assert!(patch
            .rows
            .iter()
            .any(|row| row.x == WIDTH - 2 && row.cells.len() == 1));
    }
}
