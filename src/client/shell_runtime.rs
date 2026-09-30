use super::*;

pub(super) fn dispatch_client_shell_actions(
    actions: Vec<shell::ClientShellAction>,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    endpoints: &mut endpoint::EndpointRegistry,
    mut shell: Option<&mut shell::ClientShellState>,
    detached_process_children: &mut Vec<std::process::Child>,
    scheduled_activation: &mut Option<ClientLoopEvent>,
) -> Result<(Vec<crossterm::event::MouseEvent>, bool), ClientError> {
    let mut replay_mouse = Vec::new();
    let mut repaint = false;
    for action in actions {
        match action {
            shell::ClientShellAction::Endpoint {
                endpoint_id,
                boot_id,
                request,
            } => {
                if let Some(connection) = endpoints.connection(&endpoint_id).filter(|_| {
                    endpoints.active_id() == &endpoint_id && endpoints.active_surface_available()
                }) {
                    let superseded = endpoint_commands.enqueue(
                        endpoint_id,
                        connection.generation,
                        boot_id,
                        request,
                    );
                    if let (Some(request_id), Some(shell)) = (superseded, shell.as_deref_mut()) {
                        shell.forget_superseded_request(&request_id);
                    }
                } else if let Some(shell) = shell.as_deref_mut() {
                    repaint |= shell.cancel_endpoint_request(&request.id);
                }
            }
            shell::ClientShellAction::ClipboardWrite(bytes) => {
                crate::selection::write_osc52_bytes(&bytes);
            }
            shell::ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target,
            } => {
                *scheduled_activation = Some(ClientLoopEvent::ActivateEndpoint {
                    endpoint_id,
                    target,
                    force: false,
                });
            }
            shell::ClientShellAction::OpenSafeWebUrl(url) => {
                if crate::app::actions::safe_web_url(&url).is_some() {
                    match crate::platform::open_url(&url) {
                        Ok(Some(child)) => detached_process_children.push(child),
                        Ok(None) => {}
                        Err(err) => warn!(err = %err, url = %url, "failed to open pane URL"),
                    }
                }
            }
            shell::ClientShellAction::ReplayMouse(events) => replay_mouse.extend(events),
            shell::ClientShellAction::Keybind(action) => {
                debug!(
                    ?action,
                    "client shell action awaits its presentation family"
                );
            }
        }
    }
    // A source-off-first handoff leaves the registry's committed identity pointing at a
    // deliberately surface-inactive source. Do not drain its retained queue into a server that
    // must reject it; completion below resumes the committed owner's lane.
    if endpoints.active_surface_available() {
        let active_endpoint = endpoints.active_id().clone();
        let cancelled = endpoint_commands.send_next(&active_endpoint, endpoints);
        if let Some(shell) = shell {
            for request_id in cancelled {
                repaint |= shell.cancel_endpoint_request(&request_id);
            }
        }
    }
    Ok((replay_mouse, repaint))
}

pub(super) fn client_shell_resize_message(
    shell: &shell::ClientShellState,
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    pixel_mouse: bool,
) -> ClientMessage {
    ClientMessage::ClientShellResize {
        cell_width_px,
        cell_height_px,
        surface_size: shell.surface_size(cols, rows),
        pixel_mouse,
    }
}

pub(super) fn sync_client_shell_keyboard_report_all(
    state: &mut ClientState,
) -> Result<(), ClientError> {
    let Some(shell) = state.shell.as_ref() else {
        return Ok(());
    };
    let desired = state.pane_keyboard_report_all || shell.host_keyboard_report_all_requested();
    if desired == state.keyboard_report_all_active {
        return Ok(());
    }
    crate::terminal_modes::set_host_kitty_keyboard_report_all(&mut io::stdout(), desired)
        .map_err(ClientError::ConnectionFailed)?;
    state.keyboard_report_all_active = desired;
    Ok(())
}

pub(super) fn clear_endpoint_host_effects(
    state: &mut ClientState,
    host_mouse_capture_active: &std::sync::atomic::AtomicBool,
    host_sgr_pixels_active: &std::sync::atomic::AtomicBool,
) {
    state.endpoint_mouse_capture_requested = false;
    state.endpoint_sgr_pixels_requested = false;
    let enabled = if state.shell.is_some() {
        state.shell_mouse_capture_preference
    } else {
        state.direct_mouse_capture_preference
    };
    let sgr_pixels = super::effective_sgr_pixel_mouse(enabled, false, state.pixel_geometry_exact);
    if enabled != state.mouse_capture_active
        || sgr_pixels != host_sgr_pixels_active.load(std::sync::atomic::Ordering::Acquire)
    {
        let _ = super::set_mouse_capture(enabled, sgr_pixels);
    }
    state.mouse_capture_active = enabled;
    host_mouse_capture_active.store(enabled, std::sync::atomic::Ordering::Release);
    host_sgr_pixels_active.store(sgr_pixels, std::sync::atomic::Ordering::Release);

    state.pane_keyboard_report_all = false;
    let _ = sync_client_shell_keyboard_report_all(state);
    let _ = crate::terminal_effects::write_window_title(&mut std::io::stdout(), None);
}

pub(super) fn apply_client_shell_input_source_changes(
    state: &mut ClientState,
    prefix_input_source: &mut impl crate::platform::PrefixInputSource,
) {
    let changes = state
        .shell
        .as_mut()
        .map(shell::ClientShellState::take_input_source_changes)
        .unwrap_or_default();
    for active in changes {
        if active {
            prefix_input_source.switch_to_ascii();
        } else {
            prefix_input_source.restore();
        }
    }
}

fn install_pending_activation(
    state: &mut ClientState,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    next_surface_serial: &mut u64,
    activation: endpoint::PendingEndpointActivation,
) {
    let retired = activation
        .source_command_lane()
        .map(|source| endpoint_commands.retire_lane(source))
        .unwrap_or_default();
    if let Some(shell) = state.shell.as_mut() {
        for request_id in retired {
            shell.cancel_endpoint_request(&request_id);
        }
    }
    *next_surface_serial = next_surface_serial.saturating_add(1);
    state.freeze_presentation();
    *pending = Some(activation);
}

fn local_activation_metadata_ready(
    state: &ClientState,
    endpoints: &endpoint::EndpointRegistry,
) -> bool {
    endpoints
        .connection(&endpoint::ClientEndpointId::Local)
        .is_some_and(|connection| {
            state.shell.as_ref().is_some_and(|shell| {
                shell
                    .endpoint_snapshot_identity(
                        &endpoint::ClientEndpointId::Local,
                        connection.generation,
                    )
                    .is_some()
            })
        })
}

pub(super) fn take_ready_local_activation(
    state: &mut ClientState,
    endpoints: &endpoint::EndpointRegistry,
) -> Option<ClientLoopEvent> {
    if !local_activation_metadata_ready(state, endpoints) {
        return None;
    }
    state
        .deferred_local_activation
        .take()
        .map(|intent| ClientLoopEvent::ActivateEndpoint {
            endpoint_id: intent.endpoint_id,
            target: intent.target,
            force: false,
        })
}

pub(super) fn begin_endpoint_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    next_surface_serial: &mut u64,
    endpoint_id: endpoint::ClientEndpointId,
    target: Option<shell::ClientEndpointFocusTarget>,
    force: bool,
    now: std::time::Instant,
    scheduled_activation: &mut Option<ClientLoopEvent>,
) -> Result<(), ClientError> {
    state.deferred_local_activation = None;
    if endpoint_id.is_local() && !local_activation_metadata_ready(state, endpoints) {
        state.deferred_local_activation = Some(endpoint::EndpointActivationIntent {
            endpoint_id,
            target,
        });
        if let Some(shell) = state.shell.as_mut() {
            shell.receive_endpoint_unavailable(
                "Local is reconnecting; selection will resume when it is ready".into(),
            );
        }
        return Ok(());
    }
    let replace_pending = endpoint_id.is_local()
        && pending
            .as_ref()
            .is_some_and(|activation| !activation.can_retarget(&endpoint_id));
    if !replace_pending {
        if let Some(activation) = pending.as_mut() {
            if activation.can_retarget(&endpoint_id) {
                let retarget_error = activation.retarget(target, endpoints).err();
                if let Some(error) = retarget_error {
                    rollback_endpoint_activation(state, endpoints, pending, error);
                }
            } else {
                // Once rollback starts, even a request for the original target is a new intent.
                // Retain it until restoration finishes; Local can instead abandon this handoff.
                let outcome = activation.supersede(endpoint_id, target, endpoints);
                if let endpoint::ActivationRollback::Unavailable(message) = outcome {
                    *pending = None;
                    present_handoff_unavailable(state, message);
                }
            }
            return Ok(());
        }
    }
    let already_active = !replace_pending
        && !force
        && endpoints.active_id() == &endpoint_id
        && endpoints
            .connection(&endpoint_id)
            .is_some_and(|connection| connection.surface_active);
    if already_active {
        if let (Some(shell), Some(target)) = (state.shell.as_mut(), target) {
            let actions = shell.focus_endpoint_target(target);
            let (_, repaint) = dispatch_client_shell_actions(
                actions,
                endpoint_commands,
                endpoints,
                Some(shell),
                &mut state.detached_process_children,
                scheduled_activation,
            )?;
            if repaint {
                if let Some(frame) = shell.compose(state.reported_size.0, state.reported_size.1) {
                    state.present_frame(frame);
                }
            }
        }
        return Ok(());
    }
    let Some(shell) = state.shell.as_ref() else {
        return Ok(());
    };
    let resize = client_shell_resize_message(
        shell,
        state.reported_size.0,
        state.reported_size.1,
        state.reported_cell_size.0,
        state.reported_cell_size.1,
        state.pixel_geometry_exact,
    );
    let mut warm_path = endpoint::WarmPathMiss::NotKept;
    if pending.is_none() {
        let source_id = endpoints.active_id().clone();
        let shell = state.shell.as_mut().expect("checked client shell");
        match endpoint::present_background_surface(
            shell,
            endpoints,
            &endpoint_id,
            target.as_ref(),
            &resize,
            *next_surface_serial,
        ) {
            Err(miss) => warm_path = miss,
            Ok(committed) => {
                *next_surface_serial = next_surface_serial.saturating_add(1);
                // Like a full handoff, commands still queued for the source must not land after it
                // stopped presenting.
                let retired = if source_id != endpoint_id {
                    endpoint_commands.retire_lane(&source_id)
                } else {
                    Default::default()
                };
                for request_id in retired {
                    shell.cancel_endpoint_request(&request_id);
                }
                state.replay_host_theme(endpoints, &endpoint_id);
                if let Some(event) =
                    present_committed_activation(state, endpoints, endpoint_commands, committed)?
                {
                    *scheduled_activation = Some(event);
                }
                return Ok(());
            }
        }
    }
    let Some(shell) = state.shell.as_ref() else {
        return Ok(());
    };
    match endpoint::PendingEndpointActivation::prepare(
        shell,
        endpoints,
        endpoint_id.clone(),
        target,
        resize,
        *next_surface_serial,
        warm_path,
        now,
    )
    .and_then(|activation| {
        // Preserve the old transaction if Local fails preflight. After retiring it,
        // all send failures belong to the prepared replacement's rollback path.
        if replace_pending {
            if let Some(previous) = pending.take() {
                previous.abandon(endpoints);
            }
        }
        // Ahead of surface.set(true) on the same ordered connection, so the target's first
        // frame already uses the host colors.
        state.replay_host_theme(endpoints, &endpoint_id);
        activation.start(endpoints)
    }) {
        Ok(activation) => install_pending_activation(
            state,
            endpoint_commands,
            pending,
            next_surface_serial,
            activation,
        ),
        Err(endpoint::ActivationBeginError::Preflight(error)) => {
            if let Some(shell) = state.shell.as_mut() {
                shell.receive_endpoint_unavailable(format!(
                    "{}: {error}",
                    shell.endpoint_label(&endpoint_id)
                ));
            }
        }
        Err(endpoint::ActivationBeginError::Partial { activation, error }) => {
            // A send error is not evidence that its peer did not observe the write. Freeze and
            // retain the lifecycle object before rollback so no source or target output can be
            // projected until one ownership path has been proved again.
            install_pending_activation(
                state,
                endpoint_commands,
                pending,
                next_surface_serial,
                *activation,
            );
            rollback_endpoint_activation(
                state,
                endpoints,
                pending,
                format!(
                    "{}: {error}",
                    state
                        .shell
                        .as_ref()
                        .map(|shell| shell.endpoint_label(&endpoint_id).to_owned())
                        .unwrap_or_else(|| format!("{endpoint_id:?}"))
                ),
            );
        }
    }
    Ok(())
}

pub(super) fn complete_endpoint_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
) -> Result<Option<ClientLoopEvent>, ClientError> {
    let committed = {
        let Some(activation) = pending.as_mut() else {
            return Ok(None);
        };
        let Some(shell) = state.shell.as_mut() else {
            return Ok(None);
        };
        match activation.complete(shell, endpoints) {
            Ok(committed) => committed,
            Err(error) => {
                shell.receive_endpoint_unavailable(error);
                return Ok(None);
            }
        }
    };

    let _ = pending.take();
    present_committed_activation(state, endpoints, endpoint_commands, committed)
}

/// Present a frame that was just committed, then open input and replay what the switch held.
fn present_committed_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    committed: endpoint::CommittedActivation,
) -> Result<Option<ClientLoopEvent>, ClientError> {
    endpoints.unfreeze_input();
    let endpoint::CommittedActivation {
        completion,
        endpoint_id,
        generation,
        effects,
        input,
    } = committed;
    let successor = match completion {
        endpoint::ActivationCompletion::RestoredSource {
            error,
            successor: next,
        } => {
            if next.is_none() {
                if let Some(shell) = state.shell.as_mut() {
                    shell.receive_endpoint_unavailable(error);
                }
            }
            next
        }
        endpoint::ActivationCompletion::Activated => None,
    };
    state.unfreeze_presentation();
    let input = (!input.is_empty())
        .then(|| {
            state
                .shell
                .as_ref()
                .and_then(|shell| shell.keyboard_input_message(input))
        })
        .flatten();
    if let Some(input) = input {
        write_to_server(endpoints, &input).map_err(ClientError::ConnectionLost)?;
    }
    if successor.is_none() {
        let active_endpoint = endpoints.active_id().clone();
        let cancelled = endpoint_commands.send_next(&active_endpoint, endpoints);
        if let Some(shell) = state.shell.as_mut() {
            for request_id in cancelled {
                shell.cancel_endpoint_request(&request_id);
            }
        }
    }
    let (cleanup, frame) = {
        let shell = state.shell.as_mut().expect("checked client shell");
        (
            shell.take_pending_graphics_cleanup(),
            shell.compose(state.reported_size.0, state.reported_size.1),
        )
    };
    state.present_graphics(&cleanup);
    if let Some(frame) = frame {
        state.present_frame(frame);
    }
    state
        .committed_effects
        .extend(
            effects
                .into_iter()
                .map(|message| ClientLoopEvent::ServerMessage {
                    endpoint_id: endpoint_id.clone(),
                    generation,
                    message: Box::new(message),
                }),
        );
    // A Local selection made while reconnecting is newer than this transaction's successor.
    if let Some(intent) = successor.filter(|_| state.deferred_local_activation.is_none()) {
        return Ok(Some(ClientLoopEvent::ActivateEndpoint {
            endpoint_id: intent.endpoint_id,
            target: intent.target,
            force: true,
        }));
    }
    Ok(None)
}

pub(super) fn present_handoff_unavailable(state: &mut ClientState, message: String) {
    // An unavailable committed endpoint has no presentation lease. Keep all pane input and late
    // source output blocked, while allowing this client-owned chrome frame through the freeze.
    state.freeze_presentation();
    let frame = state.shell.as_mut().and_then(|shell| {
        shell.receive_endpoint_unavailable(message);
        shell.compose(state.reported_size.0, state.reported_size.1)
    });
    if let Some(frame) = frame {
        state.present_frozen_chrome(frame);
    }
}

pub(super) fn rollback_endpoint_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    error: String,
) {
    let Some(activation) = pending.as_mut() else {
        return;
    };
    match activation.rollback(endpoints, error) {
        endpoint::ActivationRollback::Pending => state.freeze_presentation(),
        endpoint::ActivationRollback::Unavailable(message) => {
            *pending = None;
            // No endpoint has been proven safe to present. Keep pane input frozen, but render
            // the client-owned unavailable chrome rather than silently swallowing the error.
            present_handoff_unavailable(state, message);
        }
    }
}

pub(super) fn handle_endpoint_disconnect(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    supervisors: &mut endpoint::EndpointSupervisors,
    pending_activation: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_id: &endpoint::ClientEndpointId,
    generation: u64,
    now: std::time::Instant,
    notice: &str,
) -> bool {
    supervisors.disconnected(endpoint_id, generation, now);
    #[cfg(unix)]
    state.retire_endpoint_graphics(endpoint_id, generation);
    if pending_activation
        .as_ref()
        .is_some_and(|pending| pending.involves_endpoint(endpoint_id))
    {
        let outcome = pending_activation
            .as_mut()
            .expect("checked pending activation")
            .endpoint_disconnected(
                endpoints,
                endpoint_id,
                format!("endpoint connection was lost while activating {notice}"),
            );
        match outcome {
            endpoint::ActivationRollback::Pending => {}
            endpoint::ActivationRollback::Unavailable(error) => {
                *pending_activation = None;
                present_handoff_unavailable(state, error);
            }
        }
    }
    let endpoint_was_active = endpoints.active_id() == endpoint_id;
    let cancelled = endpoint_commands.disconnect(endpoint_id);
    let unavailable = state.shell.as_mut().and_then(|shell| {
        for request_id in cancelled {
            shell.cancel_endpoint_request(&request_id);
        }
        shell.mark_endpoint_disconnected(endpoint_id);
        endpoint_was_active.then(|| format!("{} {notice}", shell.endpoint_label(endpoint_id)))
    });
    if let Some(message) = unavailable {
        present_handoff_unavailable(state, message);
    } else if let Some(frame) = state
        .shell
        .as_mut()
        .and_then(|shell| shell.compose(state.reported_size.0, state.reported_size.1))
    {
        state.present_frame(frame);
    }
    endpoint_was_active
}

pub(super) fn handle_endpoint_attention(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    supervisors: &mut endpoint::EndpointSupervisors,
    pending_activation: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_id: &endpoint::ClientEndpointId,
    generation: u64,
    now: std::time::Instant,
    message: String,
) -> bool {
    endpoints.disconnect(endpoint_id);
    supervisors.record_status(
        endpoint_id,
        generation,
        endpoint::ClientEndpointStatus::Attention,
        now,
    );
    #[cfg(unix)]
    state.retire_endpoint_graphics(endpoint_id, generation);
    if pending_activation
        .as_ref()
        .is_some_and(|pending| pending.involves_endpoint(endpoint_id))
    {
        let outcome = pending_activation
            .as_mut()
            .expect("checked pending activation")
            .endpoint_disconnected(
                endpoints,
                endpoint_id,
                "endpoint reported attention while activating".into(),
            );
        if let endpoint::ActivationRollback::Unavailable(error) = outcome {
            *pending_activation = None;
            present_handoff_unavailable(state, error);
        }
    }
    let endpoint_was_active = endpoints.active_id() == endpoint_id;
    let cancelled = endpoint_commands.disconnect(endpoint_id);
    let unavailable = state.shell.as_mut().and_then(|shell| {
        for request_id in cancelled {
            shell.cancel_endpoint_request(&request_id);
        }
        shell.set_endpoint_status(endpoint_id, endpoint::ClientEndpointStatus::Attention);
        shell.set_machine_diagnostic(endpoint_id, message.clone());
        endpoint_was_active.then(|| format!("{}: {message}", shell.endpoint_label(endpoint_id)))
    });
    if let Some(message) = unavailable {
        present_handoff_unavailable(state, message);
    } else if let Some(frame) = state
        .shell
        .as_mut()
        .and_then(|shell| shell.compose(state.reported_size.0, state.reported_size.1))
    {
        state.present_frame(frame);
    }
    endpoint_was_active
}

pub(super) fn install_client_shell_snapshot(
    state: &mut ClientState,
    endpoint_id: &endpoint::ClientEndpointId,
    snapshot: Box<crate::protocol::ClientShellSnapshot>,
    projection_pending: bool,
    endpoints: &mut endpoint::EndpointRegistry,
    prefix_input_source: &mut impl crate::platform::PrefixInputSource,
) -> Result<(), ClientError> {
    let Some(connection) = endpoints.connection(endpoint_id) else {
        return Ok(());
    };
    let generation = connection.generation;
    let project_snapshot =
        !projection_pending && endpoints.active_id() == endpoint_id && connection.surface_active;
    let (composed, resize, graphics_cleanup) = if let Some(shell) = &mut state.shell {
        let waits_for_selected_surface = projection_pending
            || (endpoints.active_id() == endpoint_id
                && !project_snapshot
                && shell.has_presented_surface());
        let previous_size = shell.surface_size(state.reported_size.0, state.reported_size.1);
        if !waits_for_selected_surface {
            shell.set_endpoint_status(endpoint_id, endpoint::ClientEndpointStatus::Online);
        }
        if project_snapshot {
            shell.set_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
        } else {
            shell.cache_endpoint_snapshot_inactive_for_generation(
                endpoint_id,
                generation,
                snapshot,
            );
        }
        let graphics_cleanup = shell.take_pending_graphics_cleanup();
        let next_size = shell.surface_size(state.reported_size.0, state.reported_size.1);
        (
            shell.compose(state.reported_size.0, state.reported_size.1),
            (previous_size != next_size).then(|| {
                client_shell_resize_message(
                    shell,
                    state.reported_size.0,
                    state.reported_size.1,
                    state.reported_cell_size.0,
                    state.reported_cell_size.1,
                    state.pixel_geometry_exact,
                )
            }),
            graphics_cleanup,
        )
    } else {
        (None, None, Vec::new())
    };
    apply_client_shell_input_source_changes(state, prefix_input_source);
    state.present_graphics(&graphics_cleanup);
    if let Some(resize) = resize {
        endpoints.send_to(endpoint_id, &resize);
    }
    if let Some(frame) = composed {
        if projection_pending {
            state.present_frame(frame);
        } else {
            state.present_frozen_chrome(frame);
        }
    }
    Ok(())
}

pub(super) fn finish_client_shell_input(
    state: &mut ClientState,
    outcome: shell::ClientShellInput,
    frame: Option<super::frame_output::ComposedFrame>,
    endpoints: &mut endpoint::EndpointRegistry,
    pending_activation: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    prefix_input_source: &mut impl crate::platform::PrefixInputSource,
    scheduled_activation: &mut Option<ClientLoopEvent>,
) -> Result<bool, ClientError> {
    apply_client_shell_input_source_changes(state, prefix_input_source);
    if outcome.detach {
        let _ = write_to_server(endpoints, &ClientMessage::Detach);
        return Ok(true);
    }
    if outcome.resize {
        let shell = state.shell.as_ref().expect("shell mode remains active");
        let resize = client_shell_resize_message(
            shell,
            state.reported_size.0,
            state.reported_size.1,
            state.reported_cell_size.0,
            state.reported_cell_size.1,
            state.pixel_geometry_exact,
        );
        if let Some(activation) = pending_activation.as_mut() {
            if let Err(error) = activation.update_resize(resize, endpoints) {
                rollback_endpoint_activation(state, endpoints, pending_activation, error);
            }
        } else {
            let _ = write_to_server(endpoints, &resize);
        }
    }
    #[cfg(not(windows))]
    if outcome.query_host_appearance {
        query_host_terminal_appearance();
    }
    if outcome.query_host_theme {
        query_host_terminal_theme();
    }
    sync_client_shell_keyboard_report_all(state)?;
    let (replay, dispatch_repaint) = dispatch_client_shell_actions(
        outcome.actions,
        endpoint_commands,
        endpoints,
        state.shell.as_mut(),
        &mut state.detached_process_children,
        scheduled_activation,
    )?;
    let frame = if dispatch_repaint {
        state
            .shell
            .as_mut()
            .and_then(|shell| shell.compose(state.reported_size.0, state.reported_size.1))
    } else {
        frame
    };
    debug_assert!(
        replay.is_empty(),
        "mouse replay only follows endpoint results"
    );
    let active_endpoint_online = state
        .shell
        .as_ref()
        .is_none_or(|shell| shell.endpoint_is_online(endpoints.active_id()))
        && endpoints.active_surface_available();
    for request in outcome.requests {
        if let ClientMessage::ClientShellHostTheme { update } = &request {
            state.record_host_theme_update(update);
            if let Some(activation) = pending_activation.as_mut() {
                activation.update_host_theme(update.clone(), endpoints);
                continue;
            }
            if endpoints.active_surface_available() {
                write_to_server(endpoints, &request).map_err(ClientError::ConnectionLost)?;
            }
            continue;
        }
        // Host focus belongs to a pending target even when the source has gone offline or has
        // already had its surface revoked. Route it before the ordinary source-online gate.
        if let ClientMessage::ClientShellFocus { focused } = request {
            if let Some(activation) = pending_activation.as_mut() {
                if let Err(error) = activation.update_host_focus(focused, endpoints) {
                    rollback_endpoint_activation(state, endpoints, pending_activation, error);
                }
                continue;
            }
            if active_endpoint_online {
                write_to_server(endpoints, &ClientMessage::ClientShellFocus { focused })
                    .map_err(ClientError::ConnectionLost)?;
            }
            continue;
        }
        if let Some(activation) = pending_activation.as_mut() {
            // The frozen frame is not the one this input is for, so hold it for the commit.
            // Its pane ids come from the frozen frame; the commit retargets it.
            if let ClientMessage::ClientShellPaneInput { events, .. }
            | ClientMessage::ClientShellPopupInput { events, .. } = request
            {
                activation.buffer_input(events);
            }
            continue;
        }
        if !active_endpoint_online {
            continue;
        }
        write_to_server(endpoints, &request).map_err(ClientError::ConnectionLost)?;
    }
    if let Some(frame) = frame {
        if pending_activation.is_some() {
            state.present_frame(frame);
        } else {
            state.present_frozen_chrome(frame);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_color_reaches_server_before_first_snapshot() {
        #[derive(Clone)]
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<ClientMessage>>>);

        impl endpoint::EndpointTransport for Capture {
            fn send(&mut self, message: &ClientMessage) -> std::io::Result<()> {
                self.0.lock().unwrap().push(message.clone());
                Ok(())
            }
        }

        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut endpoints = endpoint::EndpointRegistry::new(
            Capture(sent.clone()),
            1,
            endpoint::EndpointNegotiation::new(Vec::new(), Vec::new()),
        );
        let mut state = ClientState::test_new();
        assert!(!state
            .shell
            .as_ref()
            .unwrap()
            .endpoint_is_online(&endpoint::ClientEndpointId::Local));
        let outcome = state.shell.as_mut().unwrap().handle_raw_events(vec![
            crate::raw_input::RawInputEvent::HostDefaultColor {
                kind: crate::terminal_theme::DefaultColorKind::Background,
                color: crate::terminal_theme::RgbColor {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33,
                },
            },
        ]);
        let mut pending_activation = None;
        let mut endpoint_commands = endpoint_commands::EndpointCommands::default();
        let mut prefix_input_source = crate::platform::RealPrefixInputSource::default();
        let mut scheduled_activation = None;
        finish_client_shell_input(
            &mut state,
            outcome,
            None,
            &mut endpoints,
            &mut pending_activation,
            &mut endpoint_commands,
            &mut prefix_input_source,
            &mut scheduled_activation,
        )
        .unwrap();

        assert_eq!(
            *sent.lock().unwrap(),
            vec![ClientMessage::ClientShellHostTheme {
                update: crate::protocol::ClientHostThemeUpdate::DefaultColor {
                    kind: crate::protocol::ClientHostDefaultColorKind::Background,
                    color: crate::protocol::ClientHostColor {
                        r: 0x11,
                        g: 0x22,
                        b: 0x33,
                    },
                },
            }]
        );

        let local = endpoint::ClientEndpointId::Local;
        assert!(endpoints.set_surface_active(&local, false));
        let outcome = state.shell.as_mut().unwrap().handle_raw_events(vec![
            crate::raw_input::RawInputEvent::HostDefaultColor {
                kind: crate::terminal_theme::DefaultColorKind::Foreground,
                color: crate::terminal_theme::RgbColor { r: 4, g: 5, b: 6 },
            },
        ]);
        finish_client_shell_input(
            &mut state,
            outcome,
            None,
            &mut endpoints,
            &mut pending_activation,
            &mut endpoint_commands,
            &mut prefix_input_source,
            &mut scheduled_activation,
        )
        .unwrap();
        assert_eq!(sent.lock().unwrap().len(), 1);

        assert!(endpoints.set_surface_active(&local, true));
        state.replay_host_theme(&mut endpoints, &local);
        let messages = sent.lock().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1], messages[0]);
        assert_eq!(
            messages[2],
            ClientMessage::ClientShellHostTheme {
                update: crate::protocol::ClientHostThemeUpdate::DefaultColor {
                    kind: crate::protocol::ClientHostDefaultColorKind::Foreground,
                    color: crate::protocol::ClientHostColor { r: 4, g: 5, b: 6 },
                },
            }
        );
    }
}
