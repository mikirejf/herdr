use super::*;
use crate::protocol::PaneSurfaceFrame;

/// A tab's remembered screen, shown while a focus request for that tab awaits its endpoint.
pub(super) struct PreviewedTab {
    tab_id: String,
    pub(super) pane_id: String,
    /// The request whose failure drops the preview. A newer focus request replaces it.
    request_id: String,
    /// The active snapshot with focus moved to the tab, rebuilt whenever a snapshot applies.
    pub(super) snapshot: Box<ClientShellSnapshot>,
    pub(super) surface: PaneSurfaceFrame,
}

impl ClientShellState {
    fn presentable_pane_surface(&self) -> Option<&PaneSurfaceFrame> {
        let snapshot = self.snapshot.as_deref()?;
        let surface = self.pane_surface.as_ref()?;
        (self.pending_pane_surface.is_none()
            && self.pane_surface_generation == self.active_snapshot_generation
            && surface.boot_id == snapshot.boot_id
            && surface.projection_revision == snapshot.revision)
            .then_some(surface)
    }

    /// Remembers the presented screen of the tab a focus request leaves.
    pub(super) fn remember_focused_tab_screen(&mut self) {
        let Some(tab_id) = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.focused_tab_id.clone())
        else {
            return;
        };
        let Some(surface) = self.presentable_pane_surface().cloned() else {
            return;
        };
        self.remembered_tab_screens
            .insert((self.active_endpoint_id.clone(), tab_id), surface);
    }

    /// Shows the remembered screen of `tab_id` as if `request_id` had already focused it.
    /// Returns whether a preview started.
    pub(super) fn start_tab_preview(&mut self, tab_id: &str, request_id: String) -> bool {
        // Copy mode and popups keep routing keys to the old tab's terminals.
        if self.copy_mode.is_some() || self.popup_terminal_id.is_some() {
            return false;
        }
        let Some((cols, rows)) = self.last_composed_size else {
            return false;
        };
        let size = self.surface_size(cols, rows);
        let Some(snapshot) = self.snapshot.as_deref() else {
            return false;
        };
        if snapshot.focused_tab_id.as_deref() == Some(tab_id) {
            return false;
        }
        let Some(surface) = self
            .remembered_tab_screens
            .get(&(self.active_endpoint_id.clone(), tab_id.to_owned()))
            .filter(|surface| {
                surface.boot_id == snapshot.boot_id
                    && (surface.frame.width, surface.frame.height) == (size.cols, size.rows)
            })
        else {
            return false;
        };
        let Some(pane_id) = surface
            .panes
            .iter()
            .find(|pane| pane.focused)
            .map(|pane| pane.pane_id.clone())
            .filter(|pane_id| snapshot.panes.iter().any(|pane| &pane.pane_id == pane_id))
        else {
            return false;
        };
        let Some(preview_snapshot) = previewed_snapshot(snapshot, tab_id, &pane_id) else {
            return false;
        };
        self.previewed_tab = Some(PreviewedTab {
            tab_id: tab_id.to_owned(),
            pane_id,
            request_id,
            snapshot: preview_snapshot,
            surface: surface.clone(),
        });
        self.pending_workspace_highlight = None;
        true
    }

    fn tab_preview_confirmed(&self) -> bool {
        self.previewed_tab.as_ref().is_some_and(|preview| {
            self.snapshot.as_deref().is_some_and(|snapshot| {
                snapshot.focused_tab_id.as_deref() == Some(preview.tab_id.as_str())
            }) && self.presentable_pane_surface().is_some()
        })
    }

    /// Drops the preview once the endpoint presents its tab.
    pub(super) fn confirm_tab_preview(&mut self) {
        if self.tab_preview_confirmed() {
            self.previewed_tab = None;
        }
    }

    /// Drops the preview once the endpoint presents its tab, or once the endpoint is known to
    /// focus another tab. Otherwise refreshes its chrome from the latest snapshot.
    pub(super) fn reconcile_tab_preview(&mut self) {
        let confirmed = self.tab_preview_confirmed();
        let Some(preview) = self.previewed_tab.as_ref() else {
            return;
        };
        let Some(snapshot) = self.snapshot.as_deref() else {
            self.previewed_tab = None;
            return;
        };
        let focused = snapshot.focused_tab_id.as_deref() == Some(preview.tab_id.as_str());
        let settled = !self.pending_requests.contains_key(&preview.request_id);
        let refreshed =
            (!confirmed && (focused || !settled) && preview.surface.boot_id == snapshot.boot_id)
                .then(|| previewed_snapshot(snapshot, &preview.tab_id, &preview.pane_id))
                .flatten();
        match (refreshed, self.previewed_tab.as_mut()) {
            (Some(refreshed), Some(preview)) => preview.snapshot = refreshed,
            _ => self.previewed_tab = None,
        }
    }

    /// Drops the preview when its request fails.
    pub(super) fn fail_tab_preview(&mut self, request_id: &str) {
        if self
            .previewed_tab
            .as_ref()
            .is_some_and(|preview| preview.request_id == request_id)
        {
            self.previewed_tab = None;
        }
    }

    /// Forgets remembered screens the active endpoint can no longer show.
    pub(super) fn evict_remembered_tab_screens(&mut self) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let endpoint_id = &self.active_endpoint_id;
        self.remembered_tab_screens
            .retain(|(remembered_endpoint, tab_id), surface| {
                remembered_endpoint != endpoint_id
                    || (surface.boot_id == snapshot.boot_id
                        && snapshot.tabs.iter().any(|tab| &tab.tab_id == tab_id))
            });
    }
}

/// `snapshot` with focus moved the way the endpoint moves it when it focuses `tab_id`.
fn previewed_snapshot(
    snapshot: &ClientShellSnapshot,
    tab_id: &str,
    pane_id: &str,
) -> Option<Box<ClientShellSnapshot>> {
    let workspace_id = snapshot
        .tabs
        .iter()
        .find(|tab| tab.tab_id == tab_id)?
        .workspace_id
        .clone();
    let mut preview = Box::new(snapshot.clone());
    for workspace in &mut preview.workspaces {
        workspace.focused = workspace.workspace_id == workspace_id;
        if workspace.focused {
            workspace.active_tab_id = tab_id.to_owned();
        }
    }
    for tab in &mut preview.tabs {
        tab.focused = tab.tab_id == tab_id;
    }
    for pane in &mut preview.panes {
        pane.focused = pane.pane_id == pane_id;
    }
    for agent in &mut preview.agents {
        agent.focused = agent.pane_id == pane_id;
    }
    preview.focused_workspace_id = Some(workspace_id);
    preview.focused_tab_id = Some(tab_id.to_owned());
    preview.focused_pane_id = Some(pane_id.to_owned());
    Some(preview)
}
