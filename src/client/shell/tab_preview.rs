use super::*;
use crate::protocol::{ClientSurfaceSize, PaneSurfaceFrame};

/// Tabs whose screens the active endpoint connection need not be asked for again: already
/// requested or remembered at this boot and surface size.
pub(super) struct TabScreenRequests {
    endpoint_id: ClientEndpointId,
    generation: Option<u64>,
    boot_id: String,
    size: ClientSurfaceSize,
    /// When the surface took this size. Requests wait for it to settle, so a resize drag does
    /// not ask for every tab at every intermediate size.
    size_since: std::time::Instant,
    tab_ids: HashSet<String>,
    /// The one tab whose screen was asked for and has not arrived. The next tab waits for its
    /// reply so prefetched screens never queue ahead of live surface updates.
    in_flight: Option<InFlightTabScreen>,
}

struct InFlightTabScreen {
    tab_id: String,
    /// The endpoint skips a tab that closed, became the shown tab or failed to render, without
    /// replying, so the wait for a reply must end on its own.
    deadline: std::time::Instant,
}

const TAB_SCREEN_SIZE_SETTLE: std::time::Duration = std::time::Duration::from_millis(300);
const TAB_SCREEN_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
/// Prefetched screens share the link with live echo, so no new one is asked for until the user
/// has stopped typing this long.
const TAB_SCREEN_INPUT_QUIET: std::time::Duration = std::time::Duration::from_secs(1);

/// A tab's remembered screen, shown while a focus request for that tab awaits its endpoint.
pub(super) struct PreviewedTab {
    pub(super) tab_id: String,
    pub(super) pane_id: String,
    /// The request whose failure drops the preview. A newer focus request replaces it.
    request_id: String,
    /// The active snapshot with focus moved to the tab, rebuilt whenever a snapshot applies.
    pub(super) snapshot: Box<ClientShellSnapshot>,
    pub(super) surface: PaneSurfaceFrame,
}

/// How a remembered tab screen stands in for its tab.
struct TabPreviewPlan<'a> {
    surface: &'a PaneSurfaceFrame,
    /// The pane the screen shows focused.
    shown_pane_id: String,
    /// The pane to predict focus on instead, when the request names another one.
    retarget: Option<String>,
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

    /// The remembered screen of `tab_id` on `endpoint_id`, when it was rendered at `boot_id` and
    /// `size`.
    fn remembered_tab_screen(
        &self,
        endpoint_id: &ClientEndpointId,
        tab_id: &str,
        boot_id: &str,
        size: ClientSurfaceSize,
    ) -> Option<&PaneSurfaceFrame> {
        self.remembered_tab_screens
            .get(&(endpoint_id.clone(), tab_id.to_owned()))
            .filter(|surface| {
                surface.boot_id == boot_id
                    && (surface.frame.width, surface.frame.height) == (size.cols, size.rows)
            })
    }

    /// How a remembered screen can show `endpoint_id`, presented with `snapshot` at `size`,
    /// focused on `tab_id`: with focus on `pane_id` when given, otherwise on the pane that
    /// screen shows focused.
    fn tab_preview_plan(
        &self,
        endpoint_id: &ClientEndpointId,
        snapshot: &ClientShellSnapshot,
        size: ClientSurfaceSize,
        tab_id: &str,
        pane_id: Option<&str>,
    ) -> Option<TabPreviewPlan<'_>> {
        if snapshot.focused_tab_id.as_deref() == Some(tab_id)
            || !snapshot.tabs.iter().any(|tab| tab.tab_id == tab_id)
        {
            return None;
        }
        let surface = self.remembered_tab_screen(endpoint_id, tab_id, &snapshot.boot_id, size)?;
        let shown_pane_id = surface
            .panes
            .iter()
            .find(|pane| pane.focused)
            .map(|pane| pane.pane_id.clone())
            .filter(|pane_id| snapshot.panes.iter().any(|pane| &pane.pane_id == pane_id))?;
        let retarget = match pane_id {
            Some(pane_id) if pane_id != shown_pane_id => {
                // A zoomed remembered screen may not show the pane at all.
                let shown = surface.panes.iter().any(|pane| pane.pane_id == pane_id)
                    && snapshot.panes.iter().any(|pane| pane.pane_id == pane_id);
                // The remembered screen shows another pane focused; only the client can
                // restyle it.
                if !shown || snapshot.pane_focus_style.is_none() {
                    return None;
                }
                Some(pane_id.to_owned())
            }
            _ => None,
        };
        Some(TabPreviewPlan {
            surface,
            shown_pane_id,
            retarget,
        })
    }

    /// Whether a request for `target`, made right after `endpoint_id` starts presenting
    /// `surface` with its snapshot from `generation`, shows the target at once: as a preview of
    /// a remembered tab screen, or as a pane focus prediction on `surface`. Answers before the
    /// switch what [`Self::push_endpoint_method_with_kind`] does after
    /// [`Self::activate_endpoint_projection_keeping_size`] and `surface` are applied.
    pub(crate) fn can_preview_focus_target(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        surface: &PaneSurfaceFrame,
        target: &ClientEndpointFocusTarget,
    ) -> bool {
        // Presenting `surface` restores its popup, which blocks a tab preview and keeps keys
        // and the frame on the popup instead of a predicted pane.
        if surface.popup.is_some() {
            return false;
        }
        let Some(snapshot) = self.endpoint_snapshot(endpoint_id, generation) else {
            return false;
        };
        if !self
            .endpoint_supports_method(endpoint_id, &super::actions::focus_method(target.clone()))
        {
            return false;
        }
        let Some((cols, rows)) = self.last_composed_size else {
            return false;
        };
        let size = self.surface_size_for(Some(snapshot), cols, rows);
        let previews = |tab_id: &str, pane_id: Option<&str>| {
            self.tab_preview_plan(endpoint_id, snapshot, size, tab_id, pane_id)
                .is_some()
        };
        match target {
            ClientEndpointFocusTarget::Workspace(workspace_id) => snapshot
                .workspaces
                .iter()
                .find(|workspace| &workspace.workspace_id == workspace_id)
                .is_some_and(|workspace| previews(&workspace.active_tab_id, None)),
            ClientEndpointFocusTarget::Tab(tab_id) => previews(tab_id, None),
            ClientEndpointFocusTarget::Pane(pane_id) => {
                super::pane_focus::surface_can_show_pane_focus(snapshot, surface, pane_id)
                    || snapshot
                        .panes
                        .iter()
                        .find(|pane| &pane.pane_id == pane_id)
                        .filter(|pane| snapshot.focused_tab_id.as_ref() != Some(&pane.tab_id))
                        .is_some_and(|pane| previews(&pane.tab_id, Some(pane_id)))
            }
        }
    }

    /// The tab and pane the user is shown focused.
    #[cfg(test)]
    pub(crate) fn shown_focus_for_test(&self) -> (Option<String>, Option<String>) {
        (
            self.effective_focused_tab_id().map(str::to_owned),
            self.focused_pane_id(),
        )
    }

    /// [`Self::activate_endpoint_projection`] for a switch whose next frame is composed at the
    /// size of the last one. Activating another endpoint forgets the composed size, which a tab
    /// preview needs before that frame.
    pub(crate) fn activate_endpoint_projection_keeping_size(
        &mut self,
        endpoint_id: &ClientEndpointId,
    ) -> bool {
        let size = self.last_composed_size;
        let activated = self.activate_endpoint_projection(endpoint_id);
        if activated {
            self.last_composed_size = size;
        }
        activated
    }

    /// Shows the remembered screen of `tab_id` as if `request_id` had already focused it, with
    /// focus on `pane_id` when given, otherwise on the pane that screen shows focused. Returns
    /// whether a preview started.
    pub(super) fn start_tab_preview(
        &mut self,
        tab_id: &str,
        pane_id: Option<&str>,
        request_id: String,
    ) -> bool {
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
        let Some(plan) =
            self.tab_preview_plan(&self.active_endpoint_id, snapshot, size, tab_id, pane_id)
        else {
            return false;
        };
        let Some(preview_snapshot) = previewed_snapshot(snapshot, tab_id, &plan.shown_pane_id)
        else {
            return false;
        };
        let TabPreviewPlan {
            surface,
            shown_pane_id,
            retarget,
        } = plan;
        let surface = surface.clone();
        self.previewed_tab = Some(PreviewedTab {
            tab_id: tab_id.to_owned(),
            pane_id: shown_pane_id,
            request_id: request_id.clone(),
            snapshot: preview_snapshot,
            surface,
        });
        self.pending_workspace_highlight = None;
        if let Some(pane_id) = retarget {
            self.retarget_tab_preview(pane_id, request_id);
        }
        true
    }

    /// The active endpoint's snapshot as the frame shows it, when that differs from the
    /// endpoint's own: the previewed tab's, with focus on the predicted pane of the shown tab.
    pub(super) fn shown_focus_snapshot(&self) -> Option<Box<ClientShellSnapshot>> {
        let base = match self.previewed_tab.as_ref() {
            Some(preview) => &*preview.snapshot,
            None => self.snapshot.as_deref()?,
        };
        let predicted = self
            .predicted_pane_focus
            .as_ref()
            .filter(|predicted| base.focused_pane_id.as_deref() != Some(predicted.pane_id.as_str()))
            .filter(|predicted| {
                base.panes.iter().any(|pane| {
                    pane.pane_id == predicted.pane_id
                        && base.focused_tab_id.as_deref() == Some(pane.tab_id.as_str())
                })
            })
            .and_then(|predicted| {
                previewed_snapshot(base, base.focused_tab_id.as_deref()?, &predicted.pane_id)
            });
        predicted.or_else(|| {
            self.previewed_tab
                .as_ref()
                .map(|preview| preview.snapshot.clone())
        })
    }

    /// The tab the user is looking at: the previewed tab while its focus request awaits the
    /// endpoint, otherwise the endpoint's focused tab. Requests the user means for the shown
    /// tab must name it, not the endpoint's still-unchanged focus.
    pub(super) fn effective_focused_tab_id(&self) -> Option<&str> {
        match self.previewed_tab.as_ref() {
            Some(preview) => Some(preview.tab_id.as_str()),
            None => self.snapshot.as_deref()?.focused_tab_id.as_deref(),
        }
    }

    /// The workspace of [`Self::effective_focused_tab_id`].
    pub(super) fn effective_focused_workspace_id(&self) -> Option<&str> {
        match self.previewed_tab.as_ref() {
            Some(preview) => preview.snapshot.focused_workspace_id.as_deref(),
            None => self.snapshot.as_deref()?.focused_workspace_id.as_deref(),
        }
    }

    /// How a request that may move focus affects the preview. `None`: the request leaves the
    /// preview, so it ends as for any other focus change. `Some(None)`: focus stays on the pane
    /// shown. `Some(Some(pane_id))`: focus moves to a pane of the previewed tab.
    pub(super) fn previewed_pane_target(
        &self,
        method: &crate::api::schema::Method,
    ) -> Option<Option<String>> {
        use crate::api::schema::Method;

        let preview = self.previewed_tab.as_ref()?;
        let shown = self
            .focused_pane_id()
            .unwrap_or_else(|| preview.pane_id.clone());
        let target = match method {
            Method::PaneFocus(target) => {
                let in_tab =
                    preview.snapshot.panes.iter().any(|pane| {
                        pane.pane_id == target.pane_id && pane.tab_id == preview.tab_id
                    });
                if !in_tab {
                    return None;
                }
                Some(target.pane_id.clone())
            }
            Method::PaneFocusDirection(params) => {
                let source = params.pane_id.as_deref().unwrap_or(&shown);
                super::pane_focus::pane_in_direction(&preview.surface, source, params.direction)
                    .flatten()
            }
            _ => return None,
        };
        let target = target.filter(|pane_id| *pane_id != shown);
        // Without the style the client cannot draw the new pane focused on the remembered screen.
        let can_restyle = self
            .snapshot
            .as_deref()
            .is_some_and(|snapshot| snapshot.pane_focus_style.is_some());
        if target.is_some() && !can_restyle {
            return None;
        }
        Some(target)
    }

    /// Moves focus to `pane_id` within the previewed tab, as a pane focus prediction carried by
    /// `request_id`. The prediction outlives the preview, so the endpoint confirming the tab
    /// before it handles this request does not move keys back. Returns whether the shown focus
    /// changed.
    pub(super) fn retarget_tab_preview(&mut self, pane_id: String, request_id: String) -> bool {
        let changed = self.predict_pane_focus(pane_id.clone(), request_id);
        let refreshed = self.previewed_tab.as_ref().and_then(|preview| {
            previewed_snapshot(self.snapshot.as_deref()?, &preview.tab_id, &pane_id)
        });
        if let Some(preview) = self.previewed_tab.as_mut() {
            if let Some(refreshed) = refreshed {
                preview.snapshot = refreshed;
            }
            preview.pane_id = pane_id;
        }
        changed
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

    /// The next tab of the active endpoint to ask a screen for, so a first switch to it can
    /// preview it. Each tab is asked for once per connection, boot and surface size, and only
    /// after the previous tab's reply arrived or timed out.
    pub(crate) fn take_tab_screen_request(
        &mut self,
        cols: u16,
        rows: u16,
        now: std::time::Instant,
    ) -> Option<String> {
        let size = self.surface_size(cols, rows);
        let snapshot = self.snapshot.as_deref()?;
        let previous = self.tab_screen_requests.take();
        let size_since = previous
            .as_ref()
            .filter(|requests| requests.size == size)
            .map_or(now, |requests| requests.size_since);
        let mut requests = previous
            .filter(|requests| {
                requests.endpoint_id == self.active_endpoint_id
                    && requests.generation == self.active_snapshot_generation
                    && requests.boot_id == snapshot.boot_id
                    && requests.size == size
            })
            .unwrap_or_else(|| TabScreenRequests {
                endpoint_id: self.active_endpoint_id.clone(),
                generation: self.active_snapshot_generation,
                boot_id: snapshot.boot_id.clone(),
                size,
                size_since,
                tab_ids: HashSet::new(),
                in_flight: None,
            });
        if requests
            .in_flight
            .as_ref()
            .is_some_and(|in_flight| now >= in_flight.deadline)
        {
            requests.in_flight = None;
        }
        let user_idle = self
            .last_user_input
            .is_none_or(|input| now.saturating_duration_since(input) >= TAB_SCREEN_INPUT_QUIET);
        if requests.in_flight.is_some()
            || now.saturating_duration_since(size_since) < TAB_SCREEN_SIZE_SETTLE
            || !user_idle
        {
            self.tab_screen_requests = Some(requests);
            return None;
        }
        let mut wanted = None;
        for tab in &snapshot.tabs {
            if snapshot.focused_tab_id.as_ref() == Some(&tab.tab_id)
                || requests.tab_ids.contains(&tab.tab_id)
            {
                continue;
            }
            requests.tab_ids.insert(tab.tab_id.clone());
            let remembered = self
                .remembered_tab_screen(
                    &self.active_endpoint_id,
                    &tab.tab_id,
                    &snapshot.boot_id,
                    size,
                )
                .is_some();
            if !remembered {
                requests.in_flight = Some(InFlightTabScreen {
                    tab_id: tab.tab_id.clone(),
                    deadline: now + TAB_SCREEN_REPLY_TIMEOUT,
                });
                wanted = Some(tab.tab_id.clone());
                break;
            }
        }
        self.tab_screen_requests = Some(requests);
        wanted
    }

    /// Notes that the user just gave input, which holds back the next tab screen request.
    pub(crate) fn note_user_input(&mut self, now: std::time::Instant) {
        self.last_user_input = Some(now);
    }

    /// The next moment tab screen prefetch can make progress without other input: the surface
    /// size settling, the user input going quiet, or the in-flight reply timing out.
    pub(super) fn tab_screen_request_deadline(
        &self,
        now: std::time::Instant,
    ) -> Option<std::time::Instant> {
        let requests = self.tab_screen_requests.as_ref()?;
        let settled = requests.size_since + TAB_SCREEN_SIZE_SETTLE;
        let quiet = self
            .last_user_input
            .map(|input| input + TAB_SCREEN_INPUT_QUIET);
        requests
            .in_flight
            .as_ref()
            .map(|in_flight| in_flight.deadline)
            .into_iter()
            .chain([settled])
            .chain(quiet)
            .filter(|deadline| *deadline > now)
            .min()
    }

    /// Remembers a tab screen the endpoint rendered on request, unless it is stale or the client
    /// already remembers a screen of that tab at this size.
    pub(crate) fn receive_tab_screen(
        &mut self,
        endpoint_id: &ClientEndpointId,
        tab_id: String,
        surface: PaneSurfaceFrame,
    ) {
        // A reply to an earlier request, from before a resize or reboot, must not end the wait
        // for the request that replaced it.
        if let Some(requests) = self.tab_screen_requests.as_mut().filter(|requests| {
            &requests.endpoint_id == endpoint_id
                && requests.boot_id == surface.boot_id
                && (requests.size.cols, requests.size.rows)
                    == (surface.frame.width, surface.frame.height)
        }) {
            if requests
                .in_flight
                .as_ref()
                .is_some_and(|in_flight| in_flight.tab_id == tab_id)
            {
                requests.in_flight = None;
            }
        }
        let Some((cols, rows)) = self.last_composed_size else {
            return;
        };
        let size = self.surface_size(cols, rows);
        if (surface.frame.width, surface.frame.height) != (size.cols, size.rows) {
            return;
        }
        let current = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| endpoint.snapshot.as_deref())
            .is_some_and(|snapshot| {
                snapshot.boot_id == surface.boot_id
                    && snapshot.tabs.iter().any(|tab| tab.tab_id == tab_id)
            });
        if !current {
            return;
        }
        if self
            .remembered_tab_screen(endpoint_id, &tab_id, &surface.boot_id, size)
            .is_some()
        {
            return;
        }
        self.remembered_tab_screens
            .insert((endpoint_id.clone(), tab_id), surface);
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
