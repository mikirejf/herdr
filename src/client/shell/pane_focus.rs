use super::*;
use crate::protocol::{FrameData, PaneSurfaceFrame};

/// A pane focus change the shell shows before its endpoint confirms it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PredictedPaneFocus {
    pub(super) pane_id: String,
    /// The request whose outcome decides the prediction. A newer focus request replaces it.
    pub(super) request_id: String,
}

impl ClientShellState {
    /// The pane that receives keys and pane-targeted actions: the predicted pane while a focus
    /// change is in flight, otherwise the endpoint's focused pane.
    pub(super) fn focused_pane_id(&self) -> Option<String> {
        self.predicted_pane_focus
            .as_ref()
            .map(|predicted| predicted.pane_id.clone())
            .or_else(|| {
                self.snapshot
                    .as_deref()
                    .and_then(|snapshot| snapshot.focused_pane_id.clone())
            })
    }

    /// Whether the presented surface can show focus on `pane_id` without the endpoint.
    pub(super) fn can_predict_pane_focus(&self, pane_id: &str) -> bool {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return false;
        };
        let Some(surface) = self.pane_surface.as_ref() else {
            return false;
        };
        // A zoomed surface shows one pane, and focusing another pane changes the zoom.
        let zoomed = snapshot
            .tabs
            .iter()
            .any(|tab| Some(&tab.tab_id) == snapshot.focused_tab_id.as_ref() && tab.zoomed);
        snapshot.pane_focus_style.is_some()
            && !zoomed
            && self.pending_pane_surface.is_none()
            && surface.boot_id == snapshot.boot_id
            && surface.projection_revision == snapshot.revision
            && surface.panes.iter().any(|pane| pane.pane_id == pane_id)
    }

    /// The pane the endpoint's directional focus picks from `source`, or `None` when the
    /// presented surface cannot show that focus change.
    pub(super) fn surface_pane_in_direction(
        &self,
        source: &str,
        direction: crate::api::schema::PaneDirection,
    ) -> Option<Option<String>> {
        if !self.can_predict_pane_focus(source) {
            return None;
        }
        let panes = &self.pane_surface.as_ref()?.panes;
        let infos = panes
            .iter()
            .enumerate()
            .map(|(index, pane)| crate::layout::PaneInfo {
                id: crate::layout::PaneId::from_raw(u32::try_from(index).unwrap_or(u32::MAX)),
                rect: Rect::new(pane.rect.x, pane.rect.y, pane.rect.width, pane.rect.height),
                inner_rect: Rect::default(),
                scrollbar_rect: None,
                borders: ratatui::widgets::Borders::NONE,
                is_focused: pane.pane_id == source,
            })
            .collect::<Vec<_>>();
        let source = infos.iter().find(|info| info.is_focused)?;
        Some(
            crate::layout::find_in_direction(source, direction.into(), &infos)
                .map(|id| panes[id.raw() as usize].pane_id.clone()),
        )
    }

    /// Returns whether the presented focus changed.
    pub(super) fn predict_pane_focus(&mut self, pane_id: String, request_id: String) -> bool {
        let presented = self.focused_pane_id();
        let unchanged =
            self.predicted_pane_focus.is_none() && presented.as_deref() == Some(pane_id.as_str());
        if unchanged {
            return false;
        }
        let changed = presented.as_deref() != Some(pane_id.as_str());
        if changed {
            self.previous_pane_id = presented;
        }
        self.predicted_pane_focus = Some(PredictedPaneFocus {
            pane_id,
            request_id,
        });
        changed
    }

    /// Drops the prediction once a snapshot confirms it, or once its request has finished and
    /// the endpoint chose a different pane.
    pub(super) fn reconcile_predicted_pane_focus(&mut self) {
        let Some(predicted) = self.predicted_pane_focus.as_ref() else {
            return;
        };
        let Some(snapshot) = self.snapshot.as_deref() else {
            self.predicted_pane_focus = None;
            return;
        };
        let confirmed = snapshot.focused_pane_id.as_deref() == Some(predicted.pane_id.as_str());
        let pane_exists = snapshot
            .panes
            .iter()
            .any(|pane| pane.pane_id == predicted.pane_id);
        let settled = !self.pending_requests.contains_key(&predicted.request_id);
        if confirmed || !pane_exists || settled {
            self.predicted_pane_focus = None;
        }
    }

    /// The presented surface restyled for the predicted focus, or `None` when the surface
    /// already shows the focus the shell presents.
    pub(super) fn predicted_pane_surface_frame(
        &self,
        surface: &PaneSurfaceFrame,
    ) -> Option<FrameData> {
        let predicted = self.predicted_pane_focus.as_ref()?;
        let style = self.snapshot.as_deref()?.pane_focus_style?;
        let focused = surface
            .panes
            .iter()
            .position(|pane| pane.pane_id == predicted.pane_id)?;
        if surface.panes[focused].focused {
            return None;
        }
        let mut frame = surface.frame.clone();
        crate::ui::restyle_pane_surface_focus(&mut frame, &surface.panes, focused, style);
        // Only the endpoint knows where an unfocused pane's cursor sits.
        frame.cursor = None;
        Some(frame)
    }
}
