//! Virtual rendering helpers for headless client frame streaming.

use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::layout::{Position, Rect, Size};

use crate::app::state::AppState;
use crate::protocol::render_ansi::{BlitEncoder, EncodedBlit};
use crate::protocol::{
    CursorState, FrameData, PaneSurfaceFrame, PaneSurfacePatch, RenderEncoding, ServerMessage,
    SurfaceGraphicsAssetKey, SurfaceGraphicsScene, TerminalFrame,
};
use crate::terminal::TerminalRuntimeRegistry;

/// Per-client render baseline for the negotiated render encoding.
pub(crate) enum ClientRenderState {
    /// Semantic clients compare full frame data and skip identical frames.
    Semantic {
        last_surface: Option<Box<PaneSurfaceFrame>>,
        surface_revision: u64,
        surface_reuse: bool,
        surface_delta: bool,
        surface_scroll: bool,
        recompute_pending: bool,
        /// `Some` once the client accepts per-tab baselines.
        tab_baselines: Option<TabBaselines>,
    },
    /// Terminal-ANSI clients keep a terminal diff encoder and sequence number.
    TerminalAnsi {
        blit_encoder: BlitEncoder,
        seq: u64,
        repaint_pending: bool,
    },
}

impl ClientRenderState {
    pub(crate) fn new(render_encoding: RenderEncoding) -> Self {
        match render_encoding {
            RenderEncoding::SemanticFrame => Self::Semantic {
                last_surface: None,
                surface_revision: 0,
                surface_reuse: false,
                surface_delta: false,
                surface_scroll: false,
                recompute_pending: false,
                tab_baselines: None,
            },
            RenderEncoding::TerminalAnsi => Self::TerminalAnsi {
                blit_encoder: BlitEncoder::new(),
                seq: 0,
                repaint_pending: false,
            },
        }
    }

    pub(crate) fn enable_surface_reuse(&mut self, enabled: bool) {
        if let Self::Semantic { surface_reuse, .. } = self {
            *surface_reuse = enabled;
        }
    }

    pub(crate) fn enable_surface_delta(&mut self, enabled: bool) {
        if let Self::Semantic { surface_delta, .. } = self {
            *surface_delta = enabled;
        }
    }

    pub(crate) fn enable_surface_scroll(&mut self, enabled: bool) {
        if let Self::Semantic { surface_scroll, .. } = self {
            *surface_scroll = enabled;
        }
    }

    pub(crate) fn enable_surface_tab_baselines(&mut self, enabled: bool) {
        if let Self::Semantic { tab_baselines, .. } = self {
            *tab_baselines = enabled.then(TabBaselines::default);
        }
    }

    pub(crate) fn request_recompute(&mut self) {
        if let Self::Semantic {
            surface_delta: true,
            recompute_pending,
            ..
        } = self
        {
            *recompute_pending = true;
        } else {
            self.request_repaint();
        }
    }

    pub(crate) fn requires_recompute(&self) -> bool {
        matches!(
            self,
            Self::Semantic {
                recompute_pending: true,
                ..
            }
        )
    }

    pub(crate) fn reset_baseline(&mut self) {
        match self {
            Self::Semantic { .. } => self.forget_surface(),
            Self::TerminalAnsi {
                blit_encoder,
                repaint_pending,
                ..
            } => {
                *blit_encoder = BlitEncoder::new();
                *repaint_pending = false;
            }
        }
    }

    pub(crate) fn request_repaint(&mut self) {
        match self {
            Self::Semantic { .. } => self.forget_surface(),
            Self::TerminalAnsi {
                repaint_pending, ..
            } => *repaint_pending = true,
        }
    }

    /// The client may still hold parked baselines; the next switch's `keep` drops them.
    fn forget_surface(&mut self) {
        if let Self::Semantic {
            last_surface,
            tab_baselines,
            ..
        } = self
        {
            *last_surface = None;
            if let Some(tab_baselines) = tab_baselines {
                *tab_baselines = TabBaselines::default();
            }
        }
    }

    pub(crate) fn prepare_frame(&mut self, frame: FrameData) -> Option<PreparedRender> {
        match self {
            Self::Semantic { .. } => None,
            Self::TerminalAnsi {
                blit_encoder,
                seq,
                repaint_pending,
            } => {
                if !*repaint_pending && blit_encoder.is_current(&frame) {
                    crate::render_prof::event("prepare_frame.ansi.skip_current");
                    return None;
                }
                let mut encoded = blit_encoder.encode(&frame, *repaint_pending);
                crate::render_prof::event("prepare_frame.ansi.changed");
                crate::render_prof::counter("prepare_frame.ansi.bytes", encoded.bytes.len() as u64);
                if encoded.full {
                    crate::render_prof::event("prepare_frame.ansi.full");
                } else {
                    crate::render_prof::event("prepare_frame.ansi.partial");
                }
                insert_graphics_before_sync_end(&mut encoded.bytes, &frame.graphics);
                crate::render_prof::counter(
                    "prepare_frame.graphics.bytes",
                    frame.graphics.len() as u64,
                );
                Some(PreparedRender::TerminalAnsi {
                    message: ServerMessage::Terminal(TerminalFrame {
                        seq: *seq + 1,
                        width: frame.width,
                        height: frame.height,
                        full: encoded.full,
                        bytes: encoded.bytes.clone(),
                    }),
                    frame,
                    encoded: Some(encoded),
                })
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn parked_revisions(&self) -> Vec<u64> {
        let Self::Semantic {
            tab_baselines: Some(tab_baselines),
            ..
        } = self
        else {
            return Vec::new();
        };
        let mut revisions: Vec<u64> = tab_baselines
            .parked
            .iter()
            .map(|(_, parked)| parked.surface_revision)
            .collect();
        revisions.sort_unstable();
        revisions
    }

    pub(crate) fn last_pane_surface(&self) -> Option<&PaneSurfaceFrame> {
        match self {
            Self::Semantic { last_surface, .. } => last_surface.as_deref(),
            Self::TerminalAnsi { .. } => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn prepare_pane_surface(
        &mut self,
        surface: PaneSurfaceFrame,
    ) -> Option<PreparedRender> {
        self.prepare_pane_surface_with_file(surface, None, false)
    }

    /// `tab` is the tab `surface` shows; `None` means the tab of the last surface.
    pub(crate) fn prepare_pane_surface_with_file(
        &mut self,
        mut surface: PaneSurfaceFrame,
        tab: Option<&str>,
        has_file_upload: bool,
    ) -> Option<PreparedRender> {
        let Self::Semantic {
            last_surface,
            surface_revision,
            surface_reuse,
            surface_delta,
            surface_scroll,
            recompute_pending,
            tab_baselines,
            ..
        } = self
        else {
            return None;
        };
        if !has_file_upload
            && !*recompute_pending
            && surface.graphics.assets.is_empty()
            && last_surface.as_deref().is_some_and(|last| {
                last.projection_revision == surface.projection_revision
                    && last.frame == surface.frame
                    && last.panes == surface.panes
                    && last.splits == surface.splits
                    && last.popup == surface.popup
                    && last.graphics.placements == surface.graphics.placements
                    && last.graphics.retained_assets == surface.graphics.retained_assets
            })
        {
            return None;
        }
        let tab_baselines = tab_baselines.as_ref();
        let changed_tab = tab.filter(|tab| {
            tab_baselines.is_some_and(|baselines| baselines.tab.as_deref() != Some(*tab))
        });
        // A scrolled pane that only changed rows travels as a shift even when the retained
        // patch path could not run, for example while the client's render slot was full.
        if *surface_scroll && !has_file_upload && !*recompute_pending && changed_tab.is_none() {
            let patch = last_surface.as_deref().and_then(|last| {
                crate::server::surface_diff::scroll_message(
                    last,
                    &surface,
                    surface_revision.saturating_add(1),
                )
            });
            if let Some((message, patch)) = patch {
                crate::render_prof::event("prepare_pane_surface.scroll_patch");
                return Some(PreparedRender::SemanticPatch {
                    message,
                    encoded: Some(patch),
                });
            }
        }
        surface.surface_revision = surface_revision.saturating_add(1);
        let assets = std::mem::take(&mut surface.graphics.assets);
        let queued_graphics_assets = assets.iter().map(|asset| asset.key.clone()).collect();
        let committed_surface = surface.clone();
        surface.graphics.assets = assets;
        let mut message = ServerMessage::PaneSurface(surface);
        if let (Some(tab), Some(baselines), Some(last)) =
            (changed_tab, tab_baselines, last_surface.as_deref())
        {
            if baselines.tab.is_some() {
                let (switch, delta) = baselines.switch(last, tab, &mut message);
                return Some(PreparedRender::Semantic {
                    message: delta.unwrap_or(message),
                    committed_surface: Box::new(committed_surface),
                    queued_graphics_assets,
                    changed_tab: Some(tab.to_owned()),
                    switch: Some(switch),
                });
            }
        }
        let delta = (*surface_delta)
            .then_some(last_surface.as_deref())
            .flatten()
            .and_then(|last| {
                crate::protocol::surface_delta::message(last, &mut message)
                    .map_err(|error| tracing::warn!(%error, "failed to encode surface delta"))
                    .ok()
                    .flatten()
            });
        let reused = if let ServerMessage::PaneSurface(surface) = &mut message {
            (delta.is_none() && *surface_reuse)
                .then_some(last_surface.as_deref())
                .flatten()
                .filter(|last| {
                    last.boot_id == surface.boot_id
                        && last.frame == surface.frame
                        // Popup cells are not part of the reusable grid; keep their compact codec.
                        && surface.popup.is_none()
                        && surface.graphics.assets.is_empty()
                })
                .and_then(|last| {
                    crate::protocol::surface_reuse::message(last.surface_revision, surface)
                        .map_err(|error| tracing::warn!(%error, "failed to encode surface reuse"))
                        .ok()
                        .flatten()
                })
        } else {
            None
        };
        Some(PreparedRender::Semantic {
            message: delta.or(reused).unwrap_or(message),
            committed_surface: Box::new(committed_surface),
            queued_graphics_assets,
            changed_tab: changed_tab.map(str::to_owned),
            switch: None,
        })
    }

    pub(crate) fn prepare_pane_surface_patch(
        &self,
        mut patch: PaneSurfacePatch,
    ) -> Option<PreparedRender> {
        let Self::Semantic {
            last_surface,
            surface_revision,
            surface_scroll,
            ..
        } = self
        else {
            return None;
        };
        if self.requires_recompute() {
            return None;
        }
        let last = last_surface.as_deref()?;
        if last.boot_id != patch.boot_id
            || last.projection_revision != patch.projection_revision
            || last.surface_revision != patch.base_surface_revision
        {
            return None;
        }
        let next_revision = surface_revision.saturating_add(1);
        patch.surface_revision = next_revision;
        let scrolled = (*surface_scroll)
            .then(|| crate::protocol::surface_scroll::message(last, &patch))
            .flatten();
        Some(match scrolled {
            Some(message) => PreparedRender::SemanticPatch {
                message,
                encoded: Some(Box::new(patch)),
            },
            None => PreparedRender::SemanticPatch {
                message: ServerMessage::PaneSurfacePatch(patch),
                encoded: None,
            },
        })
    }

    pub(crate) fn commit_sent_frame(&mut self, prepared: PreparedRender) {
        match (self, prepared) {
            (
                Self::Semantic {
                    last_surface,
                    surface_revision,
                    recompute_pending,
                    tab_baselines,
                    ..
                },
                PreparedRender::Semantic {
                    committed_surface,
                    changed_tab,
                    switch,
                    ..
                },
            ) => {
                *surface_revision = committed_surface.surface_revision;
                let outgoing = last_surface.replace(committed_surface);
                *recompute_pending = false;
                if let Some(tab_baselines) = tab_baselines {
                    tab_baselines.commit(outgoing, changed_tab, switch.map(|switch| switch.keep));
                }
            }
            (
                Self::Semantic {
                    last_surface,
                    surface_revision,
                    ..
                },
                PreparedRender::SemanticPatch { message, encoded },
            ) => {
                let patch = match (encoded, message) {
                    (Some(patch), _) => *patch,
                    (None, ServerMessage::PaneSurfacePatch(patch)) => patch,
                    (None, _) => unreachable!("a plain semantic patch carries its pane patch"),
                };
                let surface = last_surface
                    .as_deref_mut()
                    .expect("prepared patch baseline");
                apply_pane_surface_patch(surface, &patch);
                *surface_revision = patch.surface_revision;
            }
            (
                Self::TerminalAnsi {
                    blit_encoder,
                    seq,
                    repaint_pending,
                },
                PreparedRender::TerminalAnsi {
                    frame,
                    encoded: Some(encoded),
                    ..
                },
            ) => {
                blit_encoder.commit(frame, encoded);
                *seq += 1;
                *repaint_pending = false;
            }
            _ => {}
        }
    }
}

// Planning validates all rows and pane IDs before any send. The server does not yield
// between planning and commit, so applying the accepted patch cannot fail partway through.
pub(super) fn apply_pane_surface_patch(surface: &mut PaneSurfaceFrame, patch: &PaneSurfacePatch) {
    debug_assert_eq!(surface.boot_id, patch.boot_id);
    debug_assert_eq!(surface.projection_revision, patch.projection_revision);
    debug_assert_eq!(surface.surface_revision, patch.base_surface_revision);
    for row in &patch.rows {
        let start = usize::from(row.y) * usize::from(surface.frame.width) + usize::from(row.x);
        surface.frame.cells[start..start + row.cells.len()].clone_from_slice(&row.cells);
    }
    for updated in &patch.panes {
        let pane = surface
            .panes
            .iter_mut()
            .find(|pane| pane.pane_id == updated.pane_id)
            .expect("planned patch pane");
        pane.clone_from(updated);
    }
    surface.frame.cursor.clone_from(&patch.cursor);
    surface.surface_revision = patch.surface_revision;
}

/// Server side of per-tab baselines. Every change to `parked` happens at commit, mirroring a
/// switch message the client decodes, so both ends hold the same revisions.
#[derive(Default)]
pub(crate) struct TabBaselines {
    /// The tab the committed surface shows.
    tab: Option<String>,
    /// Oldest first, at most one per tab, never `tab`: a switch to a tab drops its entry.
    parked: Vec<(String, Box<PaneSurfaceFrame>)>,
}

impl TabBaselines {
    /// A switch to `tab` that parks `last`, plus a delta against `tab`'s parked surface when
    /// that is smaller than `full`.
    fn switch(
        &self,
        last: &PaneSurfaceFrame,
        tab: &str,
        full: &mut ServerMessage,
    ) -> (Box<PreparedSwitch>, Option<ServerMessage>) {
        use crate::protocol::surface_switch::{self, SurfaceSwitch};
        let ServerMessage::PaneSurface(surface) = &*full else {
            unreachable!("a switch starts from a full surface");
        };
        let fits = |parked: &PaneSurfaceFrame| {
            parked.boot_id == surface.boot_id
                && parked.frame.width == surface.frame.width
                && parked.frame.height == surface.frame.height
        };
        let mut keep: Vec<u64> = self
            .parked
            .iter()
            .filter(|(parked_tab, parked)| parked_tab != tab && fits(parked))
            .map(|(_, parked)| parked.surface_revision)
            .chain(fits(last).then_some(last.surface_revision))
            .collect();
        keep.drain(..keep.len().saturating_sub(surface_switch::MAX_PARKED));
        let restored = self
            .parked
            .iter()
            .find(|(parked_tab, _)| parked_tab == tab)
            .and_then(|(_, base)| {
                crate::protocol::surface_delta::message(base, full)
                    .map_err(|error| tracing::warn!(%error, "failed to encode surface delta"))
                    .ok()
                    .flatten()
                    .map(|delta| (base.surface_revision, delta))
            });
        let (restore, delta) = restored.unzip();
        let header = surface_switch::message(&SurfaceSwitch {
            keep: keep.clone(),
            restore,
        });
        (Box::new(PreparedSwitch { header, keep }), delta)
    }

    fn commit(
        &mut self,
        outgoing: Option<Box<PaneSurfaceFrame>>,
        changed_tab: Option<String>,
        keep: Option<Vec<u64>>,
    ) {
        if let Some(keep) = keep {
            if let (Some(last_tab), Some(outgoing)) = (self.tab.take(), outgoing) {
                self.parked.push((last_tab, outgoing));
            }
            self.parked
                .retain(|(_, parked)| keep.contains(&parked.surface_revision));
        }
        if changed_tab.is_some() {
            self.tab = changed_tab;
        }
    }
}

pub(crate) struct PreparedSwitch {
    header: ServerMessage,
    keep: Vec<u64>,
}

fn insert_graphics_before_sync_end(encoded: &mut Vec<u8>, graphics: &[u8]) {
    if graphics.is_empty() {
        return;
    }

    if let Some(sync_end) = crate::protocol::render_ansi::final_sync_output_end(encoded) {
        encoded.splice(sync_end..sync_end, graphics.iter().copied());
    } else {
        encoded.extend_from_slice(graphics);
    }
}

/// A prepared client render message plus any baseline state needed after send.
pub(crate) enum PreparedRender {
    Semantic {
        message: ServerMessage,
        committed_surface: Box<PaneSurfaceFrame>,
        queued_graphics_assets: Vec<SurfaceGraphicsAssetKey>,
        /// The tab `committed_surface` shows, when that differs from the last committed tab.
        changed_tab: Option<String>,
        /// A switch header that the writer must send right before `message`.
        switch: Option<Box<PreparedSwitch>>,
    },
    SemanticPatch {
        message: ServerMessage,
        /// The pane patch a compact `message` encodes; `None` when `message` is that patch.
        encoded: Option<Box<PaneSurfacePatch>>,
    },
    TerminalAnsi {
        message: ServerMessage,
        frame: FrameData,
        encoded: Option<EncodedBlit>,
    },
}

impl PreparedRender {
    /// A frame that must precede `message()` in the same write.
    pub(crate) fn switch_header(&self) -> Option<&ServerMessage> {
        match self {
            Self::Semantic {
                switch: Some(switch),
                ..
            } => Some(&switch.header),
            _ => None,
        }
    }

    pub(crate) fn message(&self) -> &ServerMessage {
        match self {
            Self::Semantic { message, .. }
            | Self::SemanticPatch { message, .. }
            | Self::TerminalAnsi { message, .. } => message,
        }
    }

    /// Graphics metadata represented by this semantic update plus only the
    /// asset keys whose pixel payloads were queued. This is independent of the
    /// selected wire codec and avoids cloning asset byte vectors.
    pub(crate) fn queued_surface_graphics(
        &self,
    ) -> Option<(&SurfaceGraphicsScene, &[SurfaceGraphicsAssetKey])> {
        match self {
            Self::Semantic {
                committed_surface,
                queued_graphics_assets,
                ..
            } => Some((&committed_surface.graphics, queued_graphics_assets)),
            Self::SemanticPatch { .. } | Self::TerminalAnsi { .. } => None,
        }
    }

    pub(crate) fn has_queued_surface_assets(&self) -> bool {
        matches!(self, Self::Semantic { queued_graphics_assets, .. } if !queued_graphics_assets.is_empty())
    }

    /// Removes the largest inline payload from a full semantic surface while
    /// preserving placement metadata. Largest-first guarantees that a fitting
    /// smaller asset is not discarded behind an oversized one. Equal sizes use
    /// deterministic scene order. Encoded delta/reuse messages return `None`; callers
    /// can invalidate that baseline and retry as a full surface.
    pub(crate) fn pop_pane_surface_asset(&mut self) -> Option<SurfaceGraphicsAssetKey> {
        let Self::Semantic {
            message: ServerMessage::PaneSurface(surface),
            queued_graphics_assets,
            ..
        } = self
        else {
            return None;
        };
        let index = surface
            .graphics
            .assets
            .iter()
            .enumerate()
            .max_by_key(|(index, asset)| (asset.data.len(), *index))?
            .0;
        let asset = surface.graphics.assets.remove(index);
        let key = asset.key;
        if let Some(index) = queued_graphics_assets
            .iter()
            .position(|queued| *queued == key)
        {
            queued_graphics_assets.remove(index);
        }
        Some(key)
    }
}

struct CursorTrackingBackend {
    inner: TestBackend,
    rendered_cursor: Option<Position>,
}

impl CursorTrackingBackend {
    fn new(width: u16, height: u16) -> Self {
        Self {
            inner: TestBackend::new(width, height),
            rendered_cursor: None,
        }
    }

    fn buffer(&self) -> &ratatui::buffer::Buffer {
        self.inner.buffer()
    }

    fn rendered_cursor(&self) -> Option<CursorState> {
        self.rendered_cursor.map(|pos| CursorState {
            x: pos.x,
            y: pos.y,
            visible: true,
            shape: 0,
        })
    }
}

impl Backend for CursorTrackingBackend {
    type Error = std::convert::Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()?;
        self.rendered_cursor = None;
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        self.inner.set_cursor_position(position)?;
        self.rendered_cursor = Some(position);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

pub(crate) type RenderedTabSurface = (
    ratatui::buffer::Buffer,
    Option<CursorState>,
    Vec<((u16, u16), String, String)>,
    crate::ui::TabSurfaceLayout,
);

/// Renders only the active tab's pane surface at an origin-relative client viewport.
pub(crate) fn render_tab_surface_virtual(
    app_state: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    layout: crate::ui::TabSurfaceLayout,
    area: Rect,
) -> RenderedTabSurface {
    let surface = crate::ui::TabSurfaceView {
        target: layout.target,
        pane_infos: &layout.pane_infos,
        split_borders: &layout.split_borders,
    };
    let cursor = crate::ui::tab_surface_cursor(app_state, terminal_runtimes, surface);
    let hyperlinks = crate::ui::tab_surface_hyperlinks(app_state, terminal_runtimes, surface);

    let backend = CursorTrackingBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend::new should never fail");
    terminal
        .draw(|frame| {
            crate::ui::render_tab_surface(app_state, terminal_runtimes, surface, frame);
        })
        .expect("render to TestBackend should never fail");

    (
        terminal.backend().buffer().clone(),
        cursor,
        hyperlinks,
        layout,
    )
}

/// Renders one server-owned terminal directly for `terminal attach` clients.
pub(crate) fn render_terminal_virtual(
    runtime: &crate::terminal::TerminalRuntime,
    area: Rect,
) -> (ratatui::buffer::Buffer, Option<CursorState>) {
    let suppress_cursor = runtime.synchronized_output_active();
    let backend = CursorTrackingBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend::new should never fail");

    terminal
        .draw(|frame| {
            runtime.render(frame, area, true);
        })
        .expect("render to TestBackend should never fail");

    let buffer = terminal.backend().buffer().clone();
    let cursor = (!suppress_cursor)
        .then(|| runtime.cursor_state(area, true))
        .flatten()
        .map(|cursor| CursorState {
            x: cursor.x,
            y: cursor.y,
            visible: cursor.visible && !crate::ui::pane_is_scrolled_back(runtime),
            shape: cursor.shape,
        })
        .or_else(|| {
            (!suppress_cursor)
                .then(|| terminal.backend().rendered_cursor())
                .flatten()
        });

    (buffer, cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ClientShellPopupSurface;

    fn popup_surface(content: &str) -> PaneSurfaceFrame {
        let pane = ratatui::buffer::Buffer::with_lines(["pane"]);
        let popup = ratatui::buffer::Buffer::with_lines([content]);
        PaneSurfaceFrame {
            boot_id: "boot-1".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame: FrameData::from_ratatui_buffer_with_hyperlinks(&pane, None, &[]),
            panes: Vec::new(),
            splits: Vec::new(),
            popup: Some(Box::new(ClientShellPopupSurface {
                terminal_id: "popup-terminal".into(),
                title: "popup".into(),
                width: None,
                height: None,
                frame: FrameData::from_ratatui_buffer_with_hyperlinks(&popup, None, &[]),
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                pixel_width: 0,
                pixel_height: 0,
            })),
            graphics: crate::protocol::SurfaceGraphicsScene::default(),
        }
    }

    #[test]
    fn surface_delta_recompute_preserves_wire_baseline_but_epoch_reset_drops_it() {
        for enabled in [false, true] {
            let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
            state.enable_surface_delta(enabled);
            let mut surface = popup_surface("popup");
            surface.popup = None;
            surface.frame = FrameData::from_ratatui_buffer(
                &ratatui::buffer::Buffer::empty(Rect::new(0, 0, 120, 40)),
                None,
            );
            let initial = state.prepare_pane_surface(surface.clone()).unwrap();
            state.commit_sent_frame(initial);
            state.request_recompute();
            assert_eq!(state.last_pane_surface().is_some(), enabled);
            assert_eq!(state.requires_recompute(), enabled);
            // A freshness request still emits a new revision when every cell is equal.
            let fresh = state.prepare_pane_surface(surface.clone()).unwrap();
            assert_eq!(
                matches!(fresh.message(), ServerMessage::EndpointControl { kind, .. }
                if kind == crate::protocol::surface_delta::MESSAGE_KIND),
                enabled
            );
            assert_eq!(
                state.requires_recompute(),
                enabled,
                "prepare must not commit"
            );
            state.commit_sent_frame(fresh);
            assert!(!state.requires_recompute());
            assert_eq!(state.last_pane_surface().unwrap().surface_revision, 2);
            state.request_repaint();
            assert!(state.last_pane_surface().is_none());
            let recovery = state.prepare_pane_surface(surface).unwrap();
            assert!(
                matches!(recovery.message(), ServerMessage::PaneSurface(frame) if frame.surface_revision == 3)
            );
        }
    }

    #[test]
    fn surface_reuse_preserves_projection_and_patch_baselines_without_resending_cells() {
        for enabled in [false, true] {
            let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
            state.enable_surface_reuse(enabled);
            let mut decoder = crate::protocol::surface_reuse::Decoder::default();
            let mut surface = popup_surface("popup");
            surface.popup = None;
            let buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 240, 100));
            surface.frame = FrameData::from_ratatui_buffer(&buffer, None);
            let initial = state.prepare_pane_surface(surface.clone()).unwrap();
            decoder.decode(initial.message().clone()).unwrap();
            state.commit_sent_frame(initial);

            surface.projection_revision += 1;
            let update = state.prepare_pane_surface(surface.clone()).unwrap();
            let mut bytes = Vec::new();
            crate::protocol::write_message(&mut bytes, update.message()).unwrap();
            if enabled {
                assert!(
                    matches!(update.message(), ServerMessage::EndpointControl { kind, .. }
                    if kind == crate::protocol::surface_reuse::MESSAGE_KIND)
                );
                assert!(
                    bytes.len() < 2000,
                    "metadata update was {} bytes",
                    bytes.len()
                );
            } else {
                assert!(matches!(update.message(), ServerMessage::PaneSurface(_)));
                assert!(bytes.len() > 100_000);
            }
            let ServerMessage::PaneSurface(decoded) =
                decoder.decode(update.message().clone()).unwrap()
            else {
                panic!("decoded full surface");
            };
            assert_eq!(decoded.frame, surface.frame);
            assert_eq!(decoded.projection_revision, surface.projection_revision);
            assert_eq!(decoded.surface_revision, 2);
            state.commit_sent_frame(update);

            let mut changed_cell = surface.frame.cells[0].clone();
            changed_cell.symbol = "x".into();
            let patch = state
                .prepare_pane_surface_patch(PaneSurfacePatch {
                    boot_id: surface.boot_id.clone(),
                    projection_revision: surface.projection_revision,
                    base_surface_revision: 2,
                    surface_revision: 0,
                    rows: vec![crate::protocol::PaneSurfacePatchRow {
                        x: 0,
                        y: 0,
                        cells: vec![changed_cell.clone()],
                    }],
                    panes: Vec::new(),
                    cursor: None,
                })
                .unwrap();
            decoder.decode(patch.message().clone()).unwrap();
            state.commit_sent_frame(patch);
            surface.frame.cells[0] = changed_cell;
            surface.projection_revision += 1;
            let update = state.prepare_pane_surface(surface.clone()).unwrap();
            let ServerMessage::PaneSurface(decoded) =
                decoder.decode(update.message().clone()).unwrap()
            else {
                panic!("decoded surface after patch");
            };
            assert_eq!(decoded.frame, surface.frame);
            assert_eq!(decoded.surface_revision, 4);
            state.commit_sent_frame(update);

            // A changed border or terminal cell must still reach the client.
            surface.frame.cells[0].symbol = "y".into();
            let changed = state.prepare_pane_surface(surface.clone()).unwrap();
            assert!(matches!(changed.message(), ServerMessage::PaneSurface(_)));
            let ServerMessage::PaneSurface(decoded) =
                decoder.decode(changed.message().clone()).unwrap()
            else {
                panic!("changed full surface");
            };
            assert_eq!(decoded.frame, surface.frame);
            state.commit_sent_frame(changed);

            state.request_repaint();
            assert!(matches!(
                state.prepare_pane_surface(surface).unwrap().message(),
                ServerMessage::PaneSurface(_)
            ));
        }
    }

    #[test]
    fn surface_reuse_keeps_popup_cells_on_the_binary_codec() {
        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        state.enable_surface_reuse(true);
        let mut surface = popup_surface("popup");
        let buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 400, 100));
        surface.popup.as_mut().unwrap().frame = FrameData::from_ratatui_buffer(&buffer, None);
        let initial = state.prepare_pane_surface(surface.clone()).unwrap();
        state.commit_sent_frame(initial);
        surface.projection_revision += 1;
        let update = state.prepare_pane_surface(surface).unwrap();
        assert!(matches!(update.message(), ServerMessage::PaneSurface(_)));
        let mut bytes = Vec::new();
        crate::protocol::write_message(&mut bytes, update.message()).unwrap();
        assert!(bytes.len() < crate::protocol::MAX_FRAME_SIZE);
    }

    #[test]
    fn surface_reuse_falls_back_when_json_metadata_exceeds_the_frame_limit() {
        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        state.enable_surface_reuse(true);
        let mut surface = popup_surface("popup");
        surface.popup = None;
        surface.frame.hyperlinks = vec!["\"".repeat(crate::protocol::MAX_FRAME_SIZE / 2)];
        let initial = state.prepare_pane_surface(surface.clone()).unwrap();
        state.commit_sent_frame(initial);
        surface.projection_revision += 1;
        let update = state.prepare_pane_surface(surface).unwrap();
        assert!(matches!(update.message(), ServerMessage::PaneSurface(_)));
        let mut bytes = Vec::new();
        crate::protocol::write_message(&mut bytes, update.message()).unwrap();
        assert!(bytes.len() < crate::protocol::MAX_FRAME_SIZE);
    }

    #[test]
    fn deferred_file_upload_keeps_identical_metadata_and_retries_without_committing() {
        for reuse in [false, true] {
            let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
            state.enable_surface_reuse(reuse);
            let surface = popup_surface("native");
            let first = state.prepare_pane_surface(surface.clone()).unwrap();
            state.commit_sent_frame(first);
            assert!(state.prepare_pane_surface(surface.clone()).is_none());
            let file = state
                .prepare_pane_surface_with_file(surface.clone(), None, true)
                .unwrap();
            let retry = state
                .prepare_pane_surface_with_file(surface.clone(), None, true)
                .unwrap();
            let config = bincode::config::standard();
            assert_eq!(
                bincode::serde::encode_to_vec(file.message(), config).unwrap(),
                bincode::serde::encode_to_vec(retry.message(), config).unwrap()
            );
            state.commit_sent_frame(retry);
            assert!(state.prepare_pane_surface(surface).is_none());
        }
    }

    #[test]
    fn popup_only_surface_changes_are_not_deduplicated() {
        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        let prepared = state
            .prepare_pane_surface(popup_surface("first"))
            .expect("initial surface");
        state.commit_sent_frame(prepared);

        assert!(state
            .prepare_pane_surface(popup_surface("second"))
            .is_some());
    }

    #[test]
    fn forced_full_surface_keeps_the_connection_revision_monotonic() {
        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        let prepared = state
            .prepare_pane_surface(popup_surface("first"))
            .expect("initial surface");
        state.commit_sent_frame(prepared);
        state.request_repaint();

        let prepared = state
            .prepare_pane_surface(popup_surface("replacement"))
            .expect("forced replacement surface");
        assert!(matches!(
            prepared.message(),
            ServerMessage::PaneSurface(surface) if surface.surface_revision == 2
        ));
        state.commit_sent_frame(prepared);
        assert_eq!(state.last_pane_surface().unwrap().surface_revision, 2);
    }

    mod tab_baselines {
        use super::*;
        use crate::protocol::surface_reuse::Decoder;
        use crate::protocol::surface_switch::{self, SurfaceSwitch};

        fn connection(tab_baselines: bool) -> (ClientRenderState, Decoder) {
            let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
            state.enable_surface_reuse(true);
            state.enable_surface_delta(true);
            state.enable_surface_tab_baselines(tab_baselines);
            (
                state,
                Decoder::new(true, false).with_tab_baselines(tab_baselines),
            )
        }

        /// A 77-row surface of varied text, so compression sizes resemble a busy agent pane.
        fn text_surface(name: &str, width: u16) -> PaneSurfaceFrame {
            let mut seed = name
                .bytes()
                .fold(7u64, |seed, byte| seed * 31 + u64::from(byte));
            let lines: Vec<String> = (0..77)
                .map(|_| {
                    (0..width)
                        .map(|_| {
                            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                            b"etaoin shrdlu cmfwyp"[(seed >> 59) as usize % 20] as char
                        })
                        .collect()
                })
                .collect();
            let buffer = ratatui::buffer::Buffer::with_lines(lines);
            let mut surface = popup_surface("popup");
            surface.popup = None;
            surface.frame = FrameData::from_ratatui_buffer(&buffer, None);
            surface
        }

        fn change_line(surface: &mut PaneSurfaceFrame, y: usize, text: &str) {
            let width = usize::from(surface.frame.width);
            for (cell, symbol) in surface.frame.cells[y * width..]
                .iter_mut()
                .zip(text.chars())
            {
                cell.symbol = symbol.to_string();
            }
        }

        fn header(prepared: &PreparedRender) -> Option<SurfaceSwitch> {
            prepared.switch_header().map(|header| {
                let ServerMessage::EndpointControl { kind, data } = header else {
                    panic!("switch header is an endpoint control");
                };
                assert_eq!(kind, surface_switch::MESSAGE_KIND);
                surface_switch::decode(data).unwrap()
            })
        }

        /// The bytes of one prepared render as the writer sends them.
        fn written(prepared: &PreparedRender) -> Vec<u8> {
            let mut bytes = Vec::new();
            for message in prepared
                .switch_header()
                .into_iter()
                .chain([prepared.message()])
            {
                crate::protocol::write_message(&mut bytes, message).unwrap();
            }
            bytes
        }

        fn deflated(bytes: &[u8]) -> usize {
            let mut encoder =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            std::io::Write::write_all(&mut encoder, bytes).unwrap();
            encoder.finish().unwrap().len()
        }

        /// Reads every frame of one write like the client reader and returns the one surface.
        fn receive(decoder: &mut Decoder, bytes: &[u8]) -> Result<PaneSurfaceFrame, String> {
            let mut reader = bytes;
            let mut surfaces = Vec::new();
            while !reader.is_empty() {
                let message = crate::protocol::read_message(
                    &mut reader,
                    crate::protocol::MAX_GRAPHICS_FRAME_SIZE,
                )
                .unwrap();
                if let Some(message) = decoder.decode_frame(message)? {
                    surfaces.push(message);
                }
            }
            match surfaces.as_slice() {
                [ServerMessage::PaneSurface(surface)] => Ok(surface.clone()),
                other => panic!("expected one surface, got {other:?}"),
            }
        }

        struct Sent {
            bytes: Vec<u8>,
            switch: Option<SurfaceSwitch>,
            body: ServerMessage,
        }

        /// Sends one surface for `tab` through the wire and checks both ends stay in lockstep.
        fn send(
            state: &mut ClientRenderState,
            decoder: &mut Decoder,
            surface: &PaneSurfaceFrame,
            tab: &str,
        ) -> Sent {
            let mut surface = surface.clone();
            surface.projection_revision += 1;
            let prepared = state
                .prepare_pane_surface_with_file(surface, Some(tab), false)
                .expect("changed surface");
            let bytes = written(&prepared);
            let decoded = receive(decoder, &bytes).unwrap();
            let sent = Sent {
                bytes,
                switch: header(&prepared),
                body: prepared.message().clone(),
            };
            state.commit_sent_frame(prepared);
            assert_eq!(Some(&decoded), state.last_pane_surface());
            assert_eq!(decoder.parked_revisions(), state.parked_revisions());
            sent
        }

        fn is_delta(message: &ServerMessage) -> bool {
            matches!(message, ServerMessage::EndpointControl { kind, .. }
                if kind == crate::protocol::surface_delta::MESSAGE_KIND)
        }

        #[test]
        fn returning_to_a_tab_sends_a_delta_against_its_parked_surface() {
            let (mut state, mut decoder) = connection(true);
            let mut a = text_surface("alpha", 310);
            a.popup = popup_surface("popup").popup;
            let b = text_surface("beta", 310);

            let first = send(&mut state, &mut decoder, &a, "a");
            assert!(first.switch.is_none());
            assert!(matches!(first.body, ServerMessage::PaneSurface(_)));
            let away = send(&mut state, &mut decoder, &b, "b");
            assert_eq!(
                away.switch,
                Some(SurfaceSwitch {
                    keep: vec![1],
                    restore: None
                })
            );
            assert!(matches!(away.body, ServerMessage::PaneSurface(_)));

            change_line(&mut a, 40, "a new line of agent output");
            a.popup.as_mut().unwrap().frame.cells[0].symbol = "x".into();
            let back = send(&mut state, &mut decoder, &a, "a");
            assert_eq!(
                back.switch,
                Some(SurfaceSwitch {
                    keep: vec![2],
                    restore: Some(1)
                })
            );
            assert!(is_delta(&back.body));
            assert!(back.bytes.len() * 20 < first.bytes.len());

            let mut plain = Vec::new();
            crate::protocol::write_message(&mut plain, &away.body).unwrap();
            assert!(away.bytes.len() < plain.len() + 100);
            eprintln!(
                "310x77 framed bytes, raw / deflated: full surface {} / {}, \
                 first-visit switch {} / {}, return switch {} / {}",
                plain.len(),
                deflated(&plain),
                away.bytes.len(),
                deflated(&away.bytes),
                back.bytes.len(),
                deflated(&back.bytes),
            );
            send(&mut state, &mut decoder, &b, "b");
        }

        #[test]
        fn a_discarded_switch_leaves_both_ends_in_lockstep() {
            let (mut state, mut decoder) = connection(true);
            let a = text_surface("alpha", 120);
            let b = text_surface("beta", 120);
            send(&mut state, &mut decoder, &a, "a");
            let discarded = state
                .prepare_pane_surface_with_file(b.clone(), Some("b"), false)
                .unwrap();
            assert!(discarded.switch_header().is_some());
            drop(discarded);
            assert!(state.parked_revisions().is_empty());
            send(&mut state, &mut decoder, &b, "b");
            let back = send(&mut state, &mut decoder, &a, "a");
            assert_eq!(back.switch.unwrap().restore, Some(1));
        }

        #[test]
        fn a_size_change_drops_parked_surfaces_of_the_old_size() {
            let (mut state, mut decoder) = connection(true);
            send(&mut state, &mut decoder, &text_surface("alpha", 120), "a");
            send(&mut state, &mut decoder, &text_surface("beta", 120), "b");
            let back = send(&mut state, &mut decoder, &text_surface("alpha", 100), "a");
            assert_eq!(
                back.switch,
                Some(SurfaceSwitch {
                    keep: Vec::new(),
                    restore: None
                })
            );
            assert!(matches!(back.body, ServerMessage::PaneSurface(_)));
        }

        #[test]
        fn a_repaint_forgets_parked_surfaces_before_the_next_switch() {
            let (mut state, mut decoder) = connection(true);
            let a = text_surface("alpha", 120);
            let b = text_surface("beta", 120);
            send(&mut state, &mut decoder, &a, "a");
            send(&mut state, &mut decoder, &b, "b");
            state.request_repaint();
            assert!(state.parked_revisions().is_empty());
            // The client keeps revision 1 until a switch tells it otherwise.
            let prepared = state
                .prepare_pane_surface_with_file(b.clone(), Some("b"), false)
                .unwrap();
            assert!(prepared.switch_header().is_none());
            assert!(matches!(prepared.message(), ServerMessage::PaneSurface(_)));
            receive(&mut decoder, &written(&prepared)).unwrap();
            state.commit_sent_frame(prepared);
            assert_eq!(decoder.parked_revisions(), vec![1]);
            let away = send(&mut state, &mut decoder, &a, "a");
            assert_eq!(
                away.switch,
                Some(SurfaceSwitch {
                    keep: vec![3],
                    restore: None
                })
            );
            let back = send(&mut state, &mut decoder, &b, "b");
            assert_eq!(back.switch.unwrap().restore, Some(3));
        }

        #[test]
        fn parked_surfaces_are_bounded_on_both_ends() {
            let (mut state, mut decoder) = connection(true);
            for tab in 0..surface_switch::MAX_PARKED + 4 {
                let name = format!("tab{tab}");
                send(&mut state, &mut decoder, &text_surface(&name, 80), &name);
            }
            assert_eq!(state.parked_revisions().len(), surface_switch::MAX_PARKED);
            let newest = send(
                &mut state,
                &mut decoder,
                &text_surface("tab10", 80),
                "tab10",
            );
            assert_eq!(newest.switch.unwrap().restore, Some(11));
            let evicted = send(&mut state, &mut decoder, &text_surface("tab0", 80), "tab0");
            assert_eq!(evicted.switch.unwrap().restore, None);
        }

        #[test]
        fn a_switch_with_inline_assets_keeps_its_surface_trimmable() {
            let (mut state, mut decoder) = connection(true);
            send(&mut state, &mut decoder, &text_surface("alpha", 80), "a");
            let mut b = text_surface("beta", 80);
            b.graphics
                .assets
                .push(crate::protocol::SurfaceGraphicsAsset {
                    key: crate::protocol::SurfaceGraphicsAssetKey {
                        source: crate::protocol::SurfaceGraphicsSource::PaneLayer {
                            pane_id: "w1:p1".into(),
                            layer_id: "image".into(),
                        },
                        image_width: 1,
                        image_height: 1,
                        format: crate::protocol::SurfaceGraphicsFormat::Rgba,
                        data_len: 4,
                        data_fingerprint: 1,
                    },
                    data: vec![0, 128, 255, 255],
                });
            let mut prepared = state
                .prepare_pane_surface_with_file(b, Some("b"), false)
                .unwrap();
            assert!(prepared.switch_header().is_some());
            assert!(prepared.pop_pane_surface_asset().is_some());
            receive(&mut decoder, &written(&prepared)).unwrap();
            state.commit_sent_frame(prepared);
            assert_eq!(decoder.parked_revisions(), state.parked_revisions());
        }

        #[test]
        fn a_malformed_switch_is_rejected_without_moving_any_baseline() {
            let (mut state, mut decoder) = connection(true);
            let mut a = text_surface("alpha", 120);
            let b = text_surface("beta", 120);
            send(&mut state, &mut decoder, &a, "a");
            send(&mut state, &mut decoder, &b, "b");
            change_line(&mut a, 3, "changed");
            a.projection_revision += 1;
            let prepared = state
                .prepare_pane_surface_with_file(a.clone(), Some("a"), false)
                .unwrap();
            assert!(is_delta(prepared.message()));
            let header =
                |keep: Vec<u64>, restore| surface_switch::message(&SurfaceSwitch { keep, restore });
            let cases = [
                header(vec![2], Some(7)),
                header(vec![9], Some(1)),
                header(vec![2, 1], Some(1)),
                header(vec![2; surface_switch::MAX_PARKED + 1], Some(1)),
                ServerMessage::EndpointControl {
                    kind: surface_switch::MESSAGE_KIND.into(),
                    data: "not-base64!".into(),
                },
            ];
            for (case, bad) in cases.into_iter().enumerate() {
                assert!(decoder.decode_frame(bad).is_err(), "case {case}");
                assert_eq!(decoder.parked_revisions(), vec![1], "case {case}");
            }
            let decoded = receive(&mut decoder, &written(&prepared)).unwrap();
            state.commit_sent_frame(prepared);
            assert_eq!(Some(&decoded), state.last_pane_surface());
            assert_eq!(decoder.parked_revisions(), vec![2]);

            // The header must be followed by its surface, and only that surface may use the
            // connection's latest revision instead of the restored baseline's.
            let (mut state, mut decoder) = connection(true);
            send(&mut state, &mut decoder, &text_surface("alpha", 120), "a");
            send(&mut state, &mut decoder, &text_surface("beta", 120), "b");
            assert!(decoder
                .decode_frame(header(vec![2], Some(1)))
                .unwrap()
                .is_none());
            assert!(decoder
                .decode_frame(ServerMessage::ReloadSoundConfig)
                .is_err());
        }

        #[test]
        fn without_the_capability_switches_stay_ordinary_surfaces() {
            let (mut state, mut decoder) = connection(false);
            let a = text_surface("alpha", 120);
            let b = text_surface("beta", 120);
            for (surface, tab) in [(&a, "a"), (&b, "b"), (&a, "a")] {
                assert!(send(&mut state, &mut decoder, surface, tab)
                    .switch
                    .is_none());
            }

            let (mut state, _) = connection(true);
            let mut legacy = Decoder::new(true, false);
            for (surface, tab) in [(&a, "a"), (&b, "b")] {
                let prepared = state
                    .prepare_pane_surface_with_file(surface.clone(), Some(tab), false)
                    .unwrap();
                let decoded = receive(&mut legacy, &written(&prepared));
                assert_eq!(
                    decoded.is_err(),
                    tab == "b",
                    "an old decoder rejects the header"
                );
                state.commit_sent_frame(prepared);
            }
        }
    }
}
