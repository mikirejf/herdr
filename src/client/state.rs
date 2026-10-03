use super::*;

#[cfg(unix)]
const MAX_RETIRED_DIRECT_GRAPHICS: usize = 64;

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RetiredDirectGraphicsMatch {
    None,
    Exact,
    Saturated,
}

#[cfg(unix)]
pub(super) struct RetiredDirectGraphics {
    generation: u64,
    transfers: Vec<(u64, u32)>,
    saturated: bool,
}

#[cfg(unix)]
impl RetiredDirectGraphics {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            transfers: Vec::new(),
            saturated: false,
        }
    }
}

/// Most surface updates one batch may hold back before it presents, so a flood of output still
/// reaches the screen at a steady rate.
pub(super) const MAX_DEFERRED_SURFACE_UPDATES: usize = 64;

/// Pane surface updates applied to the shell but not yet written to the terminal.
pub(super) enum DeferredSurfacePresentation {
    /// One retained patch, which can still take the fast patch path.
    Patch(shell::ClientComposedSurfacePatch),
    /// Anything else needs one composed frame of the current shell state.
    Compose,
}

/// State tracking for the thin client.
pub(super) struct ClientState {
    /// Stateful semantic-frame encoder used when the server sends FrameData.
    pub(super) blit_encoder: render_ansi::BlitEncoder,
    pub(super) image_files: image_files::FileTransport,
    pub(super) mouse_capture_active: bool,
    pub(super) endpoint_mouse_capture_requested: bool,
    pub(super) endpoint_sgr_pixels_requested: bool,
    /// Latest physical host theme observations, retained so an endpoint selected after the
    /// observation receives the same client-owned baseline.
    pub(super) host_theme_updates: Vec<crate::protocol::ClientHostThemeUpdate>,
    pub(super) direct_mouse_capture_preference: bool,
    pub(super) shell_mouse_capture_preference: bool,
    pub(super) direct_keyboard_protocol: crate::terminal_modes::DirectHostKeyboardState,
    pub(super) pane_keyboard_report_all: bool,
    pub(super) keyboard_report_all_active: bool,
    pub(super) reported_size: (u16, u16),
    pub(super) reported_cell_size: (u32, u32),
    pub(super) sound_config: crate::config::SoundConfig,
    pub(super) kitty_graphics_enabled: bool,
    pub(super) pixel_geometry_enabled: bool,
    pub(super) pixel_geometry_exact: bool,
    #[cfg(unix)]
    pub(super) direct_graphics_response: Arc<Mutex<direct_graphics::ResponseMatcher>>,
    #[cfg(unix)]
    pub(super) retired_direct_graphics: HashMap<endpoint::ClientEndpointId, RetiredDirectGraphics>,
    #[cfg(unix)]
    pub(super) disabled_native_graphics: HashMap<endpoint::ClientEndpointId, u64>,
    pub(super) pending_native_cleanup: Vec<u8>,
    #[cfg(unix)]
    pub(super) pending_surface_graphics: HashMap<
        (endpoint::ClientEndpointId, u64, u64, u32),
        crate::protocol::SurfaceGraphicsAssetKey,
    >,
    pub(super) attach_escape: Option<AttachEscapeState>,
    #[cfg(unix)]
    pub(super) mouse_scroll_lines: usize,
    pub(super) remote_image_paste_keys:
        Vec<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)>,
    pub(super) redraw_on_focus_gained: bool,
    pub(super) repaint_pending: bool,
    /// During a source-off-first handoff the currently blitted frame remains authoritative until
    /// an acknowledged target snapshot/surface pair commits.
    pub(super) presentation_frozen: bool,
    /// Latest explicit Local selection awaiting this client's replacement Local connection.
    pub(super) deferred_local_activation: Option<endpoint::EndpointActivationIntent>,
    /// Presentation effects a committed handoff held back, handled ahead of any new event so
    /// they apply right after the frame they describe.
    pub(super) committed_effects: std::collections::VecDeque<ClientLoopEvent>,
    pub(super) deferred_surface: Option<DeferredSurfacePresentation>,
    /// Applied updates in the held frame, reported to the profiler when it is presented.
    pub(super) deferred_surface_updates: usize,
    /// Queued surface messages drained while the frame is held, whatever their outcome.
    pub(super) deferred_surface_drained: usize,
    pub(super) draw_host_cursor: bool,
    pub(super) detached_process_children: Vec<std::process::Child>,
    pub(super) shell: Option<shell::ClientShellState>,
}

impl Drop for ClientState {
    fn drop(&mut self) {
        if self.attach_escape.is_some() {
            let _ = crate::terminal_modes::set_direct_host_keyboard_protocol(
                &mut io::stdout(),
                &mut self.direct_keyboard_protocol,
                0,
                0,
            );
        }
    }
}

impl ClientState {
    /// The modes the presenting endpoint last asked for, without this client's local overrides.
    pub(super) fn endpoint_modes(&self) -> [crate::protocol::ServerMessage; 2] {
        [
            crate::protocol::ServerMessage::MouseCapture {
                enabled: self.endpoint_mouse_capture_requested,
                sgr_pixels: self.endpoint_sgr_pixels_requested,
            },
            crate::protocol::ServerMessage::ClientShellKeyboardReportAll {
                enabled: self.pane_keyboard_report_all,
            },
        ]
    }

    #[cfg(test)]
    pub(super) fn test_new() -> Self {
        Self {
            blit_encoder: render_ansi::BlitEncoder::new(),
            image_files: image_files::FileTransport::default(),
            mouse_capture_active: false,
            endpoint_mouse_capture_requested: false,
            endpoint_sgr_pixels_requested: false,
            host_theme_updates: Vec::new(),
            direct_mouse_capture_preference: false,
            shell_mouse_capture_preference: false,
            direct_keyboard_protocol: Default::default(),
            pane_keyboard_report_all: false,
            keyboard_report_all_active: false,
            reported_size: (100, 30),
            reported_cell_size: (0, 0),
            sound_config: Default::default(),
            kitty_graphics_enabled: false,
            pixel_geometry_enabled: false,
            pixel_geometry_exact: false,
            #[cfg(unix)]
            direct_graphics_response: Default::default(),
            #[cfg(unix)]
            retired_direct_graphics: HashMap::new(),
            #[cfg(unix)]
            disabled_native_graphics: Default::default(),
            pending_native_cleanup: Vec::new(),
            #[cfg(unix)]
            pending_surface_graphics: HashMap::new(),
            attach_escape: None,
            #[cfg(unix)]
            mouse_scroll_lines: 3,
            remote_image_paste_keys: Vec::new(),
            redraw_on_focus_gained: false,
            repaint_pending: false,
            presentation_frozen: false,
            deferred_local_activation: None,
            committed_effects: Default::default(),
            deferred_surface: None,
            deferred_surface_updates: 0,
            deferred_surface_drained: 0,
            draw_host_cursor: false,
            detached_process_children: Vec::new(),
            shell: Some(shell::ClientShellState::new(
                shell::ClientShellConfig::from_config(&crate::config::Config::default()),
            )),
        }
    }

    pub(super) fn request_repaint(&mut self) {
        self.repaint_pending = true;
    }

    pub(super) fn freeze_presentation(&mut self) {
        self.presentation_frozen = true;
    }

    pub(super) fn record_host_theme_update(
        &mut self,
        update: &crate::protocol::ClientHostThemeUpdate,
    ) {
        use crate::protocol::ClientHostThemeUpdate;

        match update {
            ClientHostThemeUpdate::DefaultColor { kind, .. } => {
                self.host_theme_updates.retain(|current| {
                    !matches!(
                        current,
                        ClientHostThemeUpdate::DefaultColor {
                            kind: current_kind,
                            ..
                        } if current_kind == kind
                    )
                });
            }
            ClientHostThemeUpdate::PaletteColors(_) => self
                .host_theme_updates
                .retain(|current| !matches!(current, ClientHostThemeUpdate::PaletteColors(_))),
            ClientHostThemeUpdate::Appearance(_) => self
                .host_theme_updates
                .retain(|current| !matches!(current, ClientHostThemeUpdate::Appearance(_))),
        }
        self.host_theme_updates.push(update.clone());
    }

    /// Replay the retained physical-host baseline to an endpoint that may have missed updates
    /// while another endpoint was selected.
    pub(super) fn replay_host_theme(
        &self,
        endpoints: &mut endpoint::EndpointRegistry,
        endpoint_id: &endpoint::ClientEndpointId,
    ) {
        for update in &self.host_theme_updates {
            let _ = endpoints.send_to(
                endpoint_id,
                &crate::protocol::ClientMessage::ClientShellHostTheme {
                    update: update.clone(),
                },
            );
        }
    }

    pub(super) fn unfreeze_presentation(&mut self) {
        self.presentation_frozen = false;
        // A resize or metadata event may have happened while frozen. Force a full frame rather
        // than attempting to patch the old source frame.
        self.request_repaint();
    }

    /// Present a composed error/chrome frame while retaining the handoff input freeze. The pane
    /// cells are still the last coherent surface; only client chrome (including the error) moves.
    pub(super) fn present_frozen_chrome(
        &mut self,
        frame_data: impl Into<frame_output::ComposedFrame>,
    ) {
        let frozen = self.presentation_frozen;
        // Chrome can repaint the frozen source, but cannot retire its staged images.
        let deferred_cleanup = frozen.then(|| std::mem::take(&mut self.pending_native_cleanup));
        self.presentation_frozen = false;
        self.present_frame(frame_data);
        self.presentation_frozen = frozen;
        if let Some(cleanup) = deferred_cleanup {
            self.pending_native_cleanup = cleanup;
        }
    }

    #[cfg(unix)]
    pub(super) fn queue_native_image_cleanup(&mut self, image_id: u32) {
        crate::kitty_graphics::encode_delete_image(&mut self.pending_native_cleanup, image_id);
    }

    /// Retirement is lifecycle bookkeeping, not a presentation effect. The caller has
    /// already checked the endpoint generation, even for inactive/frozen owners.
    #[cfg(unix)]
    pub(super) fn receive_graphics_retirement(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: u64,
        transfer_id: u64,
        image_id: u32,
        owner_active: bool,
    ) {
        if transfer_id & crate::kitty_graphics::surface::NATIVE_TRANSFER_BIT != 0 {
            self.disabled_native_graphics
                .insert(endpoint_id.clone(), generation);
        }
        self.record_retired_direct_graphics(endpoint_id.clone(), generation, transfer_id, image_id);
        let upload_pending = self
            .pending_surface_graphics
            .remove(&(endpoint_id.clone(), generation, transfer_id, image_id))
            .is_some();
        // Retirement cleanup is terminal-owned. A native retirement owns the
        // ID only while its exact upload is pending: collision rejection and a
        // late post-ACK retirement must not delete the currently visible bank.
        // API/non-native retirement retains its prior ownership behavior.
        let owns_image = self.queue_retired_graphics_cleanup(
            transfer_id,
            image_id,
            upload_pending,
            owner_active,
        );
        let frame = if owns_image && owner_active {
            let frozen = self.presentation_frozen;
            let size = self.reported_size;
            self.shell.as_mut().and_then(|shell| {
                shell.retire_direct_graphics_image(image_id);
                (!frozen).then(|| shell.compose(size.0, size.1)).flatten()
            })
        } else {
            None
        };
        // A retirement repaint is a normal synchronized frame. In particular,
        // do not extract its graphics into a graphics-only swap.
        if let Some(frame) = frame {
            self.present_frame(frame);
        }
        if let Ok(mut matcher) = self.direct_graphics_response.lock() {
            matcher.retire(transfer_id);
        }
    }

    /// Returns whether a retirement owns an image that may be removed from the terminal/cache.
    /// Native rejections can retire a proposed colliding ID without ever uploading it; only an
    /// exact still-pending native upload gives that retirement ownership of the image.
    #[cfg(unix)]
    pub(super) fn queue_retired_graphics_cleanup(
        &mut self,
        transfer_id: u64,
        image_id: u32,
        upload_pending: bool,
        owner_active: bool,
    ) -> bool {
        let native = transfer_id & crate::kitty_graphics::surface::NATIVE_TRANSFER_BIT != 0;
        if native && !upload_pending {
            return false;
        }
        if native || !owner_active {
            self.queue_native_image_cleanup(image_id);
        }
        true
    }

    #[cfg(unix)]
    pub(super) fn flush_native_cleanup(
        &mut self,
        writer: &mut impl std::io::Write,
    ) -> io::Result<()> {
        if self.presentation_frozen || self.pending_native_cleanup.is_empty() {
            return Ok(());
        }
        writer.write_all(&self.pending_native_cleanup)?;
        writer.flush()?;
        self.pending_native_cleanup.clear();
        Ok(())
    }

    pub(super) fn present_graphics(&mut self, graphics: &[u8]) {
        if self.presentation_frozen || graphics.is_empty() || !self.kitty_graphics_enabled {
            return;
        }
        let mut stdout = io::stdout();
        let _ = write_encoded_frame_with_graphics(&mut stdout, &[], graphics);
        let _ = stdout.flush();
    }

    /// Records a pane surface update the shell already holds. The loop presents it through
    /// `next_held_surface_event` once no further surface message is waiting.
    pub(super) fn defer_surface_presentation(
        &mut self,
        patch: Option<shell::ClientComposedSurfacePatch>,
    ) {
        self.deferred_surface = Some(match (self.deferred_surface.is_none(), patch) {
            (true, Some(patch)) => DeferredSurfacePresentation::Patch(patch),
            _ => DeferredSurfacePresentation::Compose,
        });
        self.deferred_surface_updates += 1;
    }

    pub(super) fn surface_presentation_deferred(&self) -> bool {
        self.deferred_surface.is_some()
    }

    /// Picks the event the loop handles next. While a surface frame is held back and nothing
    /// is scheduled, it takes an already queued event from `try_next` without waiting. A
    /// surface message joins the held frame; anything else, an empty queue, or the
    /// `MAX_DEFERRED_SURFACE_UPDATES`th drained message presents it first. Every drained surface
    /// message counts, whatever endpoint sent it and whether it applies, so the loop always
    /// returns to its blocking wait (timers, health checks) within a bounded number of events.
    /// `None` means the caller should wait for the next event.
    pub(super) fn next_held_surface_event(
        &mut self,
        scheduled: Option<ClientLoopEvent>,
        activation_pending: bool,
        try_next: impl FnOnce() -> Option<ClientLoopEvent>,
    ) -> Option<ClientLoopEvent> {
        self.next_held_surface_event_to(&mut io::stdout(), scheduled, activation_pending, try_next)
    }

    fn next_held_surface_event_to(
        &mut self,
        writer: &mut impl io::Write,
        scheduled: Option<ClientLoopEvent>,
        activation_pending: bool,
        try_next: impl FnOnce() -> Option<ClientLoopEvent>,
    ) -> Option<ClientLoopEvent> {
        let drain = scheduled.is_none() && self.surface_presentation_deferred();
        let event = if drain { try_next() } else { scheduled };
        let continues = event
            .as_ref()
            .is_some_and(|event| continues_surface_batch(event, activation_pending));
        if drain && continues {
            self.deferred_surface_drained += 1;
        }
        if !continues || self.deferred_surface_drained >= MAX_DEFERRED_SURFACE_UPDATES {
            self.present_deferred_surface_to(writer);
        }
        event
    }

    fn present_deferred_surface_to(&mut self, writer: &mut impl io::Write) {
        self.deferred_surface_drained = 0;
        let Some(deferred) = self.deferred_surface.take() else {
            return;
        };
        crate::render_prof::counter(
            "client_surface_batch.updates",
            std::mem::take(&mut self.deferred_surface_updates) as u64,
        );
        let compose = match deferred {
            DeferredSurfacePresentation::Patch(patch) => {
                match self.present_surface_patch_to(writer, patch) {
                    Ok(presented) => !presented,
                    Err(error) => {
                        tracing::warn!(%error, "failed to present retained pane surface patch");
                        self.request_repaint();
                        false
                    }
                }
            }
            DeferredSurfacePresentation::Compose => true,
        };
        if !compose {
            return;
        }
        let size = self.reported_size;
        if let Some(frame) = self
            .shell
            .as_mut()
            .and_then(|shell| shell.compose(size.0, size.1))
        {
            self.try_present_frame_to(writer, frame);
        }
    }

    fn present_surface_patch_to(
        &mut self,
        writer: &mut impl io::Write,
        patch: shell::ClientComposedSurfacePatch,
    ) -> io::Result<bool> {
        if self.presentation_frozen
            || self.repaint_pending
            || (self.kitty_graphics_enabled && !self.pending_native_cleanup.is_empty())
        {
            crate::render_prof::event("client_surface_patch.fallback.repaint");
            return Ok(false);
        }
        let rows = if self.draw_host_cursor {
            let Some(rows) = self
                .blit_encoder
                .patch_rows_with_drawn_cursor(&patch.rows, patch.cursor.as_ref())
            else {
                crate::render_prof::event("client_surface_patch.fallback.drawn_cursor");
                return Ok(false);
            };
            rows
        } else {
            patch.rows
        };
        let encode_started = crate::render_prof::timer();
        let Some(encoded) =
            self.blit_encoder
                .encode_patch(&rows, patch.cursor.clone(), self.draw_host_cursor)
        else {
            crate::render_prof::event("client_surface_patch.fallback.encode");
            return Ok(false);
        };
        crate::render_prof::duration_since("client_surface_patch.encode", encode_started);
        let write_started = crate::render_prof::timer();
        if !encoded.bytes.is_empty() {
            writer.write_all(&encoded.bytes)?;
            writer.flush()?;
        }
        crate::render_prof::duration_since("client_surface_patch.write", write_started);
        let committed = self.blit_encoder.commit_patch(&rows, patch.cursor, encoded);
        crate::render_prof::event(if committed {
            "client_surface_patch.success"
        } else {
            "client_surface_patch.fallback.commit"
        });
        Ok(committed)
    }

    #[cfg(unix)]
    fn retire_pending_endpoint_graphics(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: Option<u64>,
    ) {
        let retired = self
            .pending_surface_graphics
            .keys()
            .filter(|(owner, pending_generation, _, _)| {
                owner == endpoint_id
                    && generation.is_none_or(|generation| *pending_generation == generation)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut transfer_ids = Vec::with_capacity(retired.len());
        for key in retired {
            let (_, _, transfer_id, image_id) = &key;
            if transfer_id & crate::kitty_graphics::surface::NATIVE_TRANSFER_BIT != 0 {
                self.queue_native_image_cleanup(*image_id);
            }
            transfer_ids.push(*transfer_id);
            self.pending_surface_graphics.remove(&key);
        }
        if let Ok(mut matcher) = self.direct_graphics_response.lock() {
            for transfer_id in transfer_ids {
                matcher.retire(transfer_id);
            }
        }
    }

    #[cfg(unix)]
    pub(super) fn record_retired_direct_graphics(
        &mut self,
        endpoint_id: endpoint::ClientEndpointId,
        generation: u64,
        transfer_id: u64,
        image_id: u32,
    ) {
        let retired = self
            .retired_direct_graphics
            .entry(endpoint_id)
            .or_insert_with(|| RetiredDirectGraphics::new(generation));
        if retired.generation != generation {
            *retired = RetiredDirectGraphics::new(generation);
        }
        if retired.saturated || retired.transfers.contains(&(transfer_id, image_id)) {
            return;
        }
        if retired.transfers.len() == MAX_RETIRED_DIRECT_GRAPHICS {
            // Never evict an older tombstone: a delayed file for an evicted tuple could otherwise
            // overwrite a visible image. Reject every unrecognized file until generation reset.
            retired.saturated = true;
            return;
        }
        retired.transfers.push((transfer_id, image_id));
    }

    #[cfg(unix)]
    pub(super) fn match_retired_direct_graphics(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: u64,
        transfer_id: u64,
        image_id: u32,
    ) -> RetiredDirectGraphicsMatch {
        let Some(retired) = self.retired_direct_graphics.get_mut(endpoint_id) else {
            return RetiredDirectGraphicsMatch::None;
        };
        if retired.generation != generation {
            return RetiredDirectGraphicsMatch::None;
        }
        if let Some(index) = retired
            .transfers
            .iter()
            .position(|transfer| *transfer == (transfer_id, image_id))
        {
            retired.transfers.swap_remove(index);
            if retired.transfers.is_empty() && !retired.saturated {
                self.retired_direct_graphics.remove(endpoint_id);
            }
            return RetiredDirectGraphicsMatch::Exact;
        }
        if retired.saturated {
            RetiredDirectGraphicsMatch::Saturated
        } else {
            RetiredDirectGraphicsMatch::None
        }
    }

    /// Forget graphics guards only for the connection generation which actually disconnected.
    /// A delayed disconnect from an older generation must not clear its replacement's guards.
    #[cfg(unix)]
    pub(super) fn retire_endpoint_graphics(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: u64,
    ) {
        self.retire_pending_endpoint_graphics(endpoint_id, Some(generation));
        if self.disabled_native_graphics.get(endpoint_id) == Some(&generation) {
            self.disabled_native_graphics.remove(endpoint_id);
        }
        if self
            .retired_direct_graphics
            .get(endpoint_id)
            .is_some_and(|retired| retired.generation == generation)
        {
            self.retired_direct_graphics.remove(endpoint_id);
        }
    }

    /// Reset bounded per-endpoint state when a replacement connection becomes authoritative.
    #[cfg(unix)]
    pub(super) fn start_endpoint_graphics_generation(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: u64,
    ) {
        self.retire_pending_endpoint_graphics(endpoint_id, None);
        if self.disabled_native_graphics.get(endpoint_id) != Some(&generation) {
            self.disabled_native_graphics.remove(endpoint_id);
        }
        if self
            .retired_direct_graphics
            .get(endpoint_id)
            .is_some_and(|retired| retired.generation != generation)
        {
            self.retired_direct_graphics.remove(endpoint_id);
        }
    }

    #[cfg(unix)]
    pub(super) fn forget_endpoint_graphics(&mut self, endpoint_id: &endpoint::ClientEndpointId) {
        self.retire_pending_endpoint_graphics(endpoint_id, None);
        self.disabled_native_graphics.remove(endpoint_id);
        self.retired_direct_graphics.remove(endpoint_id);
    }

    pub(super) fn present_frame(&mut self, frame_data: impl Into<frame_output::ComposedFrame>) {
        let _ = self.try_present_frame(frame_data);
    }

    fn write_composed_output(
        &mut self,
        writer: &mut impl io::Write,
        encoded: &[u8],
        mut graphics: crate::kitty_graphics::GraphicsOutput,
    ) -> io::Result<()> {
        if self.kitty_graphics_enabled {
            if !self.pending_native_cleanup.is_empty() {
                graphics.operations.insert(
                    0,
                    crate::kitty_graphics::GraphicsOperation::Bytes(
                        self.pending_native_cleanup.clone(),
                    ),
                );
            }
            frame_output::write_composed_frame(
                writer.by_ref(),
                encoded,
                &graphics,
                &mut self.image_files,
            )?;
        } else {
            writer.write_all(encoded)?;
        }
        writer.flush()?;
        if self.kitty_graphics_enabled {
            self.pending_native_cleanup.clear();
        }
        Ok(())
    }

    /// Presents and commits a frame only after all terminal output has been written successfully.
    /// Callers which acknowledge presentation-sensitive work use the return value rather than
    /// treating composition as presentation.
    pub(super) fn try_present_frame(
        &mut self,
        frame_data: impl Into<frame_output::ComposedFrame>,
    ) -> bool {
        self.try_present_frame_to(&mut io::stdout(), frame_data)
    }

    fn try_present_frame_to(
        &mut self,
        writer: &mut impl io::Write,
        frame_data: impl Into<frame_output::ComposedFrame>,
    ) -> bool {
        if self.presentation_frozen {
            return false;
        }
        let frame_output::ComposedFrame {
            frame: frame_data,
            graphics,
        } = frame_data.into();
        let frame_data = if self.draw_host_cursor {
            render_ansi::frame_with_drawn_cursor(frame_data)
        } else {
            frame_data
        };
        let encoded = if self.draw_host_cursor {
            self.blit_encoder
                .encode_with_suppressed_visible_cursor(&frame_data, self.repaint_pending)
        } else {
            self.blit_encoder.encode(&frame_data, self.repaint_pending)
        };
        if let Err(error) = self.write_composed_output(writer, &encoded.bytes, graphics) {
            tracing::warn!(%error, "failed to present client frame");
            self.repaint_pending = true;
            return false;
        }
        self.blit_encoder.commit(frame_data, encoded);
        self.repaint_pending = false;
        true
    }
}

#[cfg(test)]
mod surface_batch_tests {
    use super::*;
    use crate::client::shell::tests::{snapshot, surface};

    /// Terminal stand-in. Every present ends in one flush, so flushes count frames.
    #[derive(Default)]
    struct Terminal {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl io::Write for Terminal {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    impl Terminal {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.bytes).into_owned()
        }
    }

    fn presented_state() -> ClientState {
        let mut state = ClientState::test_new();
        let shell = state.shell.as_mut().expect("test shell");
        shell.set_snapshot(Box::new(snapshot()));
        shell.set_pane_surface(surface());
        state.defer_surface_presentation(None);
        let mut terminal = Terminal::default();
        state.present_deferred_surface_to(&mut terminal);
        assert_eq!(terminal.flushes, 1, "the first surface is presented");
        state
    }

    fn patch(base_surface_revision: u64, symbol: &str) -> crate::protocol::PaneSurfacePatch {
        crate::protocol::PaneSurfacePatch {
            boot_id: "boot-1".into(),
            projection_revision: 1,
            base_surface_revision,
            surface_revision: base_surface_revision + 1,
            rows: vec![crate::protocol::PaneSurfacePatchRow {
                x: 0,
                y: 0,
                cells: vec![
                    crate::protocol::CellData {
                        symbol: symbol.into(),
                        fg: 0,
                        bg: 0,
                        modifier: 0,
                        skip: false,
                        hyperlink: None,
                    };
                    4
                ],
            }],
            panes: surface().panes,
            cursor: None,
        }
    }

    fn apply(state: &mut ClientState, patch: crate::protocol::PaneSurfacePatch) {
        let shell = state.shell.as_mut().expect("test shell");
        let shell::ClientPaneSurfacePatchOutcome::Applied(composed) =
            shell.apply_pane_surface_patch(patch)
        else {
            panic!("patch applies to the presented surface");
        };
        state.defer_surface_presentation(composed);
    }

    #[test]
    fn queued_surface_patches_present_as_one_terminal_write() {
        let mut state = presented_state();
        for (base, symbol) in [(1, "α"), (2, "β"), (3, "γ")] {
            apply(&mut state, patch(base, symbol));
        }
        assert!(matches!(
            state.deferred_surface,
            Some(DeferredSurfacePresentation::Compose)
        ));

        let mut terminal = Terminal::default();
        state.present_deferred_surface_to(&mut terminal);

        assert_eq!(terminal.flushes, 1);
        let text = terminal.text();
        assert!(text.contains('γ'), "the latest cells reach the terminal");
        assert!(
            !text.contains('α') && !text.contains('β'),
            "superseded cells are never written: {text:?}"
        );
        assert!(!state.surface_presentation_deferred());
        assert_eq!(state.deferred_surface_updates, 0);

        let mut later = Terminal::default();
        state.present_deferred_surface_to(&mut later);
        assert_eq!(later.flushes, 0, "a presented batch is not written twice");
    }

    #[test]
    fn a_single_surface_patch_keeps_the_fast_patch_output() {
        let mut batched = presented_state();
        let mut direct = presented_state();

        apply(&mut batched, patch(1, "α"));
        assert!(matches!(
            batched.deferred_surface,
            Some(DeferredSurfacePresentation::Patch(_))
        ));
        let mut batched_terminal = Terminal::default();
        batched.present_deferred_surface_to(&mut batched_terminal);

        let shell::ClientPaneSurfacePatchOutcome::Applied(Some(composed)) = direct
            .shell
            .as_mut()
            .expect("test shell")
            .apply_pane_surface_patch(patch(1, "α"))
        else {
            panic!("fast retained patch");
        };
        let mut direct_terminal = Terminal::default();
        assert!(direct
            .present_surface_patch_to(&mut direct_terminal, composed)
            .expect("patch written"));

        assert_eq!(batched_terminal.flushes, 1);
        assert_eq!(batched_terminal.bytes, direct_terminal.bytes);
    }

    #[test]
    fn a_complete_surface_and_its_patches_present_as_one_composed_frame() {
        let mut state = presented_state();
        let mut replacement = surface();
        replacement.surface_revision = 5;
        state
            .shell
            .as_mut()
            .expect("test shell")
            .set_pane_surface(replacement);
        state.defer_surface_presentation(None);
        apply(&mut state, patch(5, "δ"));

        let mut terminal = Terminal::default();
        state.present_deferred_surface_to(&mut terminal);

        assert_eq!(terminal.flushes, 1);
        assert!(terminal.text().contains('δ'));
    }

    const ACTIVE: u64 = 1;
    const BACKGROUND: u64 = 2;

    /// A queued surface message. The generation only tells `drive` how to handle it.
    fn queued_surface(generation: u64) -> ClientLoopEvent {
        ClientLoopEvent::ServerMessage {
            endpoint_id: endpoint::ClientEndpointId::Local,
            generation,
            message: Box::new(crate::protocol::ServerMessage::PaneSurface(surface())),
        }
    }

    /// Runs the client loop's event choice over `queue`. Active messages apply a patch and hold
    /// the frame like the loop's surface handlers; background messages change nothing on screen.
    /// Returns, for each terminal write, how many events were taken while the frame was held.
    fn drive(
        state: &mut ClientState,
        terminal: &mut Terminal,
        revision: &mut u64,
        queue: impl IntoIterator<Item = ClientLoopEvent>,
    ) -> Vec<usize> {
        let mut queue = queue.into_iter().collect::<std::collections::VecDeque<_>>();
        let mut held_events = 0;
        let mut writes = Vec::new();
        loop {
            let held = state.surface_presentation_deferred();
            let flushes = terminal.flushes;
            let event = state
                .next_held_surface_event_to(terminal, None, false, || queue.pop_front())
                // The loop's blocking wait.
                .or_else(|| queue.pop_front());
            if held && event.is_some() {
                held_events += 1;
            }
            if terminal.flushes > flushes {
                writes.push(std::mem::take(&mut held_events));
            }
            let Some(ClientLoopEvent::ServerMessage { generation, .. }) = event else {
                return writes;
            };
            if generation == ACTIVE {
                apply(state, patch(*revision, "α"));
                *revision += 1;
            }
        }
    }

    #[test]
    fn queued_background_surfaces_present_the_held_frame_at_the_cap() {
        let mut state = presented_state();
        apply(&mut state, patch(1, "α"));
        let mut terminal = Terminal::default();

        let writes = drive(
            &mut state,
            &mut terminal,
            &mut 2,
            (0..MAX_DEFERRED_SURFACE_UPDATES + 40).map(|_| queued_surface(BACKGROUND)),
        );

        assert_eq!(writes, vec![MAX_DEFERRED_SURFACE_UPDATES]);
        assert!(terminal.text().contains('α'));
        assert!(!state.surface_presentation_deferred());
        assert_eq!(state.deferred_surface_drained, 0);
    }

    #[test]
    fn mixed_queued_surfaces_write_once_per_capped_batch() {
        let mut state = presented_state();
        apply(&mut state, patch(1, "α"));
        let mut terminal = Terminal::default();
        let total = 3 * MAX_DEFERRED_SURFACE_UPDATES;

        let writes = drive(
            &mut state,
            &mut terminal,
            &mut 2,
            (0..total)
                .map(|index| queued_surface(if index % 3 == 0 { ACTIVE } else { BACKGROUND })),
        );

        assert!(
            writes
                .iter()
                .all(|&events| events <= MAX_DEFERRED_SURFACE_UPDATES),
            "{writes:?}"
        );
        assert_eq!(writes.len(), 3, "{writes:?}");
        assert_eq!(terminal.flushes, writes.len());
        assert!(!state.surface_presentation_deferred());
    }

    #[test]
    fn queued_active_surfaces_present_as_one_write_when_the_queue_empties() {
        let mut state = presented_state();
        apply(&mut state, patch(1, "α"));
        let mut terminal = Terminal::default();

        let writes = drive(
            &mut state,
            &mut terminal,
            &mut 2,
            (0..3).map(|_| queued_surface(ACTIVE)),
        );

        assert_eq!(writes, vec![3]);
        assert_eq!(terminal.flushes, 1);
    }
}

#[cfg(all(test, unix))]
mod native_cleanup_tests {
    use super::*;

    #[test]
    fn retired_graphics_tombstones_survive_handoff_and_only_exact_files_consume_them() {
        let mut state = ClientState::test_new();
        let endpoint = endpoint::ClientEndpointId::Local;
        state.record_retired_direct_graphics(endpoint.clone(), 7, 11, 1234);
        state.record_retired_direct_graphics(endpoint.clone(), 7, 12, 5678);
        state.disabled_native_graphics.insert(endpoint.clone(), 7);

        // Switching away and back does not reset guards, and an unrelated queued file cannot
        // consume either of two sequential API retirement tombstones.
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 7, 13, 9999),
            RetiredDirectGraphicsMatch::None
        );
        assert_eq!(
            state
                .retired_direct_graphics
                .get(&endpoint)
                .unwrap()
                .transfers
                .len(),
            2
        );
        assert_eq!(state.disabled_native_graphics.get(&endpoint), Some(&7));
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 7, 11, 1234),
            RetiredDirectGraphicsMatch::Exact
        );
        assert!(state
            .retired_direct_graphics
            .get(&endpoint)
            .unwrap()
            .transfers
            .contains(&(12, 5678)));
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 7, 12, 5678),
            RetiredDirectGraphicsMatch::Exact
        );
        assert!(!state.retired_direct_graphics.contains_key(&endpoint));
    }

    #[test]
    fn replacement_generation_resets_bounded_graphics_guards() {
        let mut state = ClientState::test_new();
        let endpoint = endpoint::ClientEndpointId::Local;
        let native = crate::kitty_graphics::surface::NATIVE_TRANSFER_BIT | 11;
        state.record_retired_direct_graphics(endpoint.clone(), 7, native, 1234);
        state.disabled_native_graphics.insert(endpoint.clone(), 7);

        state.start_endpoint_graphics_generation(&endpoint, 8);

        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 7, native, 1234),
            RetiredDirectGraphicsMatch::None
        );
        assert!(!state.disabled_native_graphics.contains_key(&endpoint));
    }

    #[test]
    fn retired_graphics_tombstone_overflow_fails_closed_until_generation_reset() {
        let mut state = ClientState::test_new();
        let endpoint = endpoint::ClientEndpointId::Local;
        for transfer_id in 0..=MAX_RETIRED_DIRECT_GRAPHICS as u64 {
            state.record_retired_direct_graphics(
                endpoint.clone(),
                7,
                transfer_id,
                transfer_id as u32,
            );
        }
        let retired = state.retired_direct_graphics.get(&endpoint).unwrap();
        assert_eq!(retired.transfers.len(), MAX_RETIRED_DIRECT_GRAPHICS);
        assert!(retired.saturated);
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 7, 1, 1),
            RetiredDirectGraphicsMatch::Exact
        );
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 7, 10_000, 10_000),
            RetiredDirectGraphicsMatch::Saturated
        );
        assert!(
            state
                .retired_direct_graphics
                .get(&endpoint)
                .unwrap()
                .saturated
        );

        state.start_endpoint_graphics_generation(&endpoint, 8);
        assert!(!state.retired_direct_graphics.contains_key(&endpoint));
        state.record_retired_direct_graphics(endpoint.clone(), 8, 1, 2);
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 8, 9, 9),
            RetiredDirectGraphicsMatch::None
        );
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 8, 1, 2),
            RetiredDirectGraphicsMatch::Exact
        );
    }

    #[test]
    fn pending_none_native_retirement_does_not_touch_cleanup_or_shell_cache() {
        let mut state = ClientState::test_new();
        state.pending_native_cleanup.extend_from_slice(b"existing");
        let transfer = crate::kitty_graphics::surface::NATIVE_TRANSFER_BIT | 7;

        assert!(!state.queue_retired_graphics_cleanup(transfer, 1234, false, true));
        assert_eq!(state.pending_native_cleanup, b"existing");
        assert!(state
            .shell
            .as_mut()
            .expect("test shell")
            .take_pending_graphics_cleanup()
            .is_empty());
    }

    #[test]
    fn queued_cleanup_is_synchronized_and_respects_graphics_capability() {
        let mut state = ClientState::test_new();
        state.kitty_graphics_enabled = true;
        state.queue_native_image_cleanup(42);
        let encoded = b"\x1b[?2026htext\x1b[?2026l";
        let mut output = Vec::new();
        state
            .write_composed_output(&mut output, encoded, Default::default())
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.find("\x1b[?2026h").unwrap() < text.find("a=d,d=I,i=42").unwrap());
        assert!(text.find("a=d,d=I,i=42").unwrap() < text.find("\x1b[?2026l").unwrap());
        assert!(state.pending_native_cleanup.is_empty());
        state.kitty_graphics_enabled = false;
        state.queue_native_image_cleanup(43);
        let mut output = Vec::new();
        state
            .write_composed_output(&mut output, encoded, Default::default())
            .unwrap();
        assert_eq!(output, encoded);
        assert!(!state.pending_native_cleanup.is_empty());
        state.kitty_graphics_enabled = true;
        let mut no_capacity = &mut [][..];
        assert!(state
            .write_composed_output(&mut no_capacity, encoded, Default::default())
            .is_err());
        assert!(!state.pending_native_cleanup.is_empty());
    }

    #[test]
    fn active_api_retirement_leaves_cleanup_to_composition() {
        let mut state = ClientState::test_new();
        assert!(state.queue_retired_graphics_cleanup(7, 1234, true, true));
        assert!(state.pending_native_cleanup.is_empty());
        assert!(state.queue_retired_graphics_cleanup(8, 5678, true, false));
        assert!(!state.pending_native_cleanup.is_empty());
    }

    #[test]
    fn unacknowledged_native_upload_cleanup_survives_frozen_disconnect() {
        let mut state = ClientState::test_new();
        let endpoint = endpoint::ClientEndpointId::Local;
        let key = crate::protocol::SurfaceGraphicsAssetKey {
            source: crate::protocol::SurfaceGraphicsSource::Terminal {
                target: crate::protocol::SurfaceGraphicsTarget::Pane {
                    pane_id: "p".into(),
                },
                image_id: 1,
            },
            image_width: 1,
            image_height: 1,
            format: crate::protocol::SurfaceGraphicsFormat::Rgba,
            data_len: 4,
            data_fingerprint: 1,
        };
        let transfer = crate::kitty_graphics::surface::NATIVE_TRANSFER_BIT | 2;
        state
            .pending_surface_graphics
            .insert((endpoint.clone(), 9, transfer, 1234), key);
        state.presentation_frozen = true;
        state.retire_endpoint_graphics(&endpoint, 9);
        assert!(state.pending_surface_graphics.is_empty());
        let mut output = Vec::new();
        state.flush_native_cleanup(&mut output).unwrap();
        assert!(output.is_empty());
        assert!(!state.pending_native_cleanup.is_empty());
        state.unfreeze_presentation();
        state.flush_native_cleanup(&mut output).unwrap();
        assert!(String::from_utf8(output.clone())
            .unwrap()
            .contains("a=d,d=I,i=1234"));
        state.flush_native_cleanup(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap().matches("a=d,").count(),
            1
        );
    }

    #[test]
    fn unacknowledged_native_upload_retirement_survives_frozen_handoff() {
        let mut state = ClientState::test_new();
        let endpoint = endpoint::ClientEndpointId::Local;
        let key = crate::protocol::SurfaceGraphicsAssetKey {
            source: crate::protocol::SurfaceGraphicsSource::Terminal {
                target: crate::protocol::SurfaceGraphicsTarget::Pane {
                    pane_id: "p".into(),
                },
                image_id: 1,
            },
            image_width: 1,
            image_height: 1,
            format: crate::protocol::SurfaceGraphicsFormat::Rgba,
            data_len: 4,
            data_fingerprint: 1,
        };
        let transfer = crate::kitty_graphics::surface::NATIVE_TRANSFER_BIT | 2;
        state
            .pending_surface_graphics
            .insert((endpoint.clone(), 9, transfer, 1234), key);
        state.presentation_frozen = true;
        // The source connection is still live, but no longer owns the active shell.
        state.receive_graphics_retirement(&endpoint, 8, transfer, 1234, false);
        assert_eq!(state.pending_surface_graphics.len(), 1);
        assert!(state.pending_native_cleanup.is_empty());
        state.receive_graphics_retirement(&endpoint, 9, transfer, 1234, false);
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 9, transfer, 1234),
            RetiredDirectGraphicsMatch::Exact
        );
        assert!(state
            .shell
            .as_mut()
            .unwrap()
            .take_pending_graphics_cleanup()
            .is_empty());
        assert!(state.pending_surface_graphics.is_empty());
        let mut output = Vec::new();
        state.flush_native_cleanup(&mut output).unwrap();
        assert!(output.is_empty());
        assert!(!state.pending_native_cleanup.is_empty());
        let cleanup = state.pending_native_cleanup.clone();
        let frame = crate::protocol::FrameData::from_ratatui_buffer(
            &ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 1, 1)),
            None,
        );
        state.kitty_graphics_enabled = true;
        assert!(!state.try_present_frame(frame.clone()));
        assert_eq!(state.pending_native_cleanup, cleanup);
        state.present_frozen_chrome(frame);
        assert!(state.presentation_frozen);
        assert_eq!(state.pending_native_cleanup, cleanup);
        state.unfreeze_presentation();
        state.flush_native_cleanup(&mut output).unwrap();
        assert!(String::from_utf8(output.clone())
            .unwrap()
            .contains("a=d,d=I,i=1234"));
        state.flush_native_cleanup(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap().matches("a=d,").count(),
            1
        );
    }
}
