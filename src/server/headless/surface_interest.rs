use super::*;

impl HeadlessServer {
    /// Apply a client-shell surface lease and return the resulting projection floor.
    ///
    /// Every `active: true` request advances the floor, even if the server already considered
    /// the connection active. That makes a new client activation epoch distinguishable from a
    /// delayed same-boot PaneSurface that was prepared for an earlier epoch. The raised floor is
    /// also what makes the next render send a fresh snapshot, since the cached one is older.
    pub(super) fn set_client_shell_surface_active(
        &mut self,
        client_id: u64,
        active: bool,
    ) -> Option<(bool, u64)> {
        let client = self.clients.get(&client_id)?;
        if !client.is_shell_client() {
            return None;
        }
        if active {
            return Some((true, self.take_client_shell_presentation(client_id, true)));
        }
        let revision = client.shell_projection_revision;
        if !client.streams_shell_surface() {
            return Some((false, revision));
        }
        self.release_client_shell_presentation(client_id, false);
        Some((true, revision))
    }

    /// Give up the presentation lease but keep this connection's surface stream on its current
    /// baseline. Only a presented surface can move to the background.
    pub(super) fn move_client_shell_surface_to_background(&mut self, client_id: u64) -> bool {
        if !self
            .clients
            .get(&client_id)
            .is_some_and(ClientConnection::is_active_shell_client)
        {
            return false;
        }
        self.release_client_shell_presentation(client_id, true);
        // The client applied these modes while presenting, but it keeps a background surface's
        // modes separately, so the stream starts them over.
        let client = self
            .clients
            .get_mut(&client_id)
            .expect("checked shell client");
        client.host_mouse_capture_active = None;
        client.host_sgr_pixels_active = None;
        client.host_keyboard_report_all_active = None;
        true
    }

    /// Take the presentation lease for a background surface. The client already holds the
    /// streamed surface, so its baseline and projection epoch carry over. Any other connection
    /// gets a fresh epoch, exactly like `client_shell.surface.set(true)`.
    pub(super) fn bring_client_shell_surface_to_foreground(&mut self, client_id: u64) -> bool {
        let Some(client) = self
            .clients
            .get(&client_id)
            .filter(|client| client.is_shell_client())
        else {
            return false;
        };
        let fresh_epoch = !client.shell_surface_background;
        self.take_client_shell_presentation(client_id, fresh_epoch);
        true
    }

    fn take_client_shell_presentation(&mut self, client_id: u64, fresh_epoch: bool) -> u64 {
        let focus_before = self.shell_focus_targets();
        let focused_tabs_before = self.focused_shell_tabs();
        let (changed, projection_revision) = {
            let client = self
                .clients
                .get_mut(&client_id)
                .expect("checked shell client");
            let changed = !client.shell_surface_active;
            client.shell_surface_active = true;
            client.shell_surface_background = false;
            // Graphics a background surface carried inline never reached the client's
            // presentation, and native uploads resume only with the lease.
            client.shell_graphics_delivery = Default::default();
            if fresh_epoch {
                client.shell_projection_revision =
                    client.shell_projection_revision.saturating_add(1);
                client.request_repaint();
                // The client holds this endpoint's effects until it commits the new frame, and
                // that frame must arrive with its modes. Reset the dedupe state so the
                // activation reply carries them even when runtime demand is unchanged.
                client.host_mouse_capture_active = None;
                client.host_sgr_pixels_active = None;
                client.host_keyboard_report_all_active = None;
                client.clear_deferred_render();
            }
            (changed, client.shell_projection_revision)
        };
        if changed {
            self.finish_shell_location_reconciliation(focus_before, &focused_tabs_before);
        }

        self.promote_client_to_foreground(client_id);
        self.sent_window_title = None;
        self.resize_shared_runtime_to_effective_size_with_pending_agent_resumes(true);
        let focused_viewer_already_owns_tab =
            self.shell_tab_id_for_client(client_id)
                .is_some_and(|tab_id| {
                    self.clients.iter().any(|(&other_id, client)| {
                        other_id != client_id
                            && client.is_active_shell_client()
                            && client.outer_terminal_focus == Some(true)
                            && self.shell_tab_id_for_client(other_id).as_deref()
                                == Some(tab_id.as_str())
                    })
                });
        if !focused_viewer_already_owns_tab {
            self.claim_shell_tab_geometry(client_id, true);
        }
        projection_revision
    }

    /// Release the presentation lease. With `keep_stream`, the connection keeps its render
    /// baseline and any frame already queued for it, because the client keeps following the
    /// stream; otherwise the stream stops and the next surface starts from nothing.
    fn release_client_shell_presentation(&mut self, client_id: u64, keep_stream: bool) {
        let focus_before = self.shell_focus_targets();
        let focused_tabs_before = self.focused_shell_tabs();
        let was_active = {
            let client = self
                .clients
                .get_mut(&client_id)
                .expect("checked shell client");
            let was_active = client.shell_surface_active;
            client.shell_surface_active = false;
            client.shell_surface_background = keep_stream;
            if !keep_stream {
                client.request_repaint();
                client.shell_graphics_delivery = Default::default();
                client.clear_deferred_render();
                if let Some(writer) = &client.writer {
                    writer.discard_pending_render();
                }
            }
            was_active
        };
        if !was_active {
            return;
        }
        let held_inputs = self
            .clients
            .get_mut(&client_id)
            .expect("checked client")
            .drain_shell_held_inputs();
        self.release_client_shell_inputs(client_id, held_inputs);
        self.retire_native_graphics_for_client(client_id);
        self.finish_shell_location_reconciliation(focus_before, &focused_tabs_before);

        self.tab_geometry_controllers
            .retain(|_, controller_id| *controller_id != client_id);
        if self.foreground_client_id == Some(client_id) {
            self.promote_latest_remaining_client();
            self.resize_shared_runtime_to_effective_size_with_pending_agent_resumes(true);
        } else {
            self.sync_foreground_client_state();
        }
        self.reapply_controlled_shell_tab_geometry(true);
    }
}
