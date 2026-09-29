use super::*;

fn endpoint() -> ClientEndpointId {
    ClientEndpointId::Ssh(
        super::super::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
    )
}

fn lease(id: ClientEndpointId, generation: u64, boot: &str) -> EndpointLease {
    EndpointLease {
        endpoint_id: id,
        generation,
        boot_id: boot.into(),
        minimum_revision: 0,
    }
}

#[derive(Clone)]
struct FakeTransport {
    sent: std::sync::Arc<std::sync::Mutex<Vec<crate::protocol::ClientMessage>>>,
    fail_after_write: bool,
}

impl super::super::EndpointTransport for FakeTransport {
    fn send(&mut self, message: &crate::protocol::ClientMessage) -> std::io::Result<()> {
        self.sent.lock().unwrap().push(message.clone());
        if self.fail_after_write {
            Err(std::io::Error::other("simulated observed write failure"))
        } else {
            Ok(())
        }
    }
}

fn negotiation() -> super::super::EndpointNegotiation {
    super::super::EndpointNegotiation::new(
        vec!["client_shell.surface.set".into()],
        vec![
            crate::protocol::endpoint::SURFACE_INTEREST_CAPABILITY.into(),
            crate::protocol::endpoint::SURFACE_ACTIVATION_EFFECTS_CAPABILITY.into(),
            crate::protocol::endpoint::PANE_FOCUS_STYLE_CAPABILITY.into(),
            crate::protocol::endpoint::SURFACE_BACKGROUND_CAPABILITY.into(),
        ],
    )
}

fn test_snapshot(boot_id: &str, revision: u64) -> crate::protocol::ClientShellSnapshot {
    crate::protocol::ClientShellSnapshot {
        boot_id: boot_id.into(),
        revision,
        config_diagnostic: None,
        product_announcement: None,
        update_available: None,
        update_install_command: String::new(),
        server_keybindings_toml: None,
        latest_release_notes_available: false,
        integration_updates_available: false,
        worktree_directory: String::new(),
        release_notes: None,
        focused_workspace_id: None,
        focused_tab_id: None,
        focused_pane_id: None,
        tab_bar_right: Vec::new(),
        tab_bar_right_separator: String::new(),
        agent_view_label: None,
        agent_order: Vec::new(),
        workspaces: Vec::new(),
        tabs: Vec::new(),
        panes: Vec::new(),
        agents: Vec::new(),
        commands: Vec::new(),
        pane_focus_style: None,
    }
}

type SentMessages = std::sync::Arc<std::sync::Mutex<Vec<crate::protocol::ClientMessage>>>;
type TestFixture = (
    crate::client::ClientShellState,
    EndpointRegistry,
    SentMessages,
    SentMessages,
);

fn shell_and_registry() -> TestFixture {
    shell_and_registry_with_failures(false, false)
}

fn shell_and_registry_with_failures(
    source_fail_after_write: bool,
    target_fail_after_write: bool,
) -> TestFixture {
    let mut shell = crate::client::ClientShellState::new(
        crate::client::ClientShellConfig::from_config(&crate::config::Config::default()),
    );
    let profile = super::super::SavedSshEndpoint {
        id: super::super::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "Remote".into(),
        target: "dev@example.com".into(),
        session: "main".into(),
        enabled: true,
    };
    let target = ClientEndpointId::Ssh(profile.id.clone());
    shell.set_endpoint_catalog(&[profile]);
    shell.set_snapshot(Box::new(test_snapshot("local-boot", 1)));
    shell.set_endpoint_status(&target, ClientEndpointStatus::Online);
    shell.set_endpoint_snapshot(&target, Box::new(test_snapshot("remote-boot", 1)));

    let local_sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let remote_sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut endpoints = EndpointRegistry::new(
        FakeTransport {
            sent: local_sent.clone(),
            fail_after_write: source_fail_after_write,
        },
        1,
        negotiation(),
    );
    endpoints.insert(
        target,
        FakeTransport {
            sent: remote_sent.clone(),
            fail_after_write: target_fail_after_write,
        },
        7,
        negotiation(),
        false,
    );
    (shell, endpoints, local_sent, remote_sent)
}

fn surface_success(id: &str, active: bool, projection_revision: u64) -> Vec<u8> {
    serde_json::to_vec(&crate::api::schema::SuccessResponse {
        id: id.into(),
        result: crate::api::schema::ResponseResult::ClientShellSurfaceSet {
            active,
            projection_revision,
        },
    })
    .unwrap()
}

fn workspace_focus_success(id: &str, workspace_id: &str) -> Vec<u8> {
    serde_json::to_vec(&crate::api::schema::SuccessResponse {
        id: id.into(),
        result: crate::api::schema::ResponseResult::WorkspaceInfo {
            workspace: crate::api::schema::WorkspaceInfo {
                workspace_id: workspace_id.into(),
                number: 1,
                label: workspace_id.into(),
                focused: true,
                pane_count: 1,
                tab_count: 1,
                active_tab_id: "tab".into(),
                agent_status: crate::api::schema::AgentStatus::Unknown,
                tokens: Default::default(),
                worktree: None,
            },
        },
    })
    .unwrap()
}

fn failure(id: &str, message: &str) -> Vec<u8> {
    serde_json::to_vec(&crate::api::schema::ErrorResponse {
        id: id.into(),
        error: crate::api::schema::ErrorBody {
            code: "surface_rejected".into(),
            message: message.into(),
        },
    })
    .unwrap()
}

fn surface_set_active(message: &crate::protocol::ClientMessage) -> Option<bool> {
    let crate::protocol::ClientMessage::ClientShellEndpointRequest { request, .. } = message else {
        return None;
    };
    let request: crate::api::schema::Request = serde_json::from_str(request).ok()?;
    match request.method {
        crate::api::schema::Method::ClientShellSurfaceSet(params) => Some(params.active),
        _ => None,
    }
}

fn resize() -> crate::protocol::ClientMessage {
    crate::protocol::ClientMessage::ClientShellResize {
        cell_width_px: 8,
        cell_height_px: 16,
        surface_size: crate::protocol::ClientSurfaceSize { cols: 80, rows: 24 },
        pixel_mouse: false,
    }
}

fn surface(boot_id: &str, revision: u64, pane: &str) -> crate::protocol::PaneSurfaceFrame {
    crate::protocol::PaneSurfaceFrame {
        boot_id: boot_id.into(),
        projection_revision: revision,
        surface_revision: revision,
        frame: crate::protocol::FrameData {
            cells: Vec::new(),
            width: 80,
            height: 24,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        },
        panes: vec![crate::protocol::PaneSurfacePane {
            pane_id: pane.into(),
            content_revision: revision,
            rect: crate::protocol::SurfaceRect {
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            inner_rect: crate::protocol::SurfaceRect {
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            scrollbar_rect: None,
            scroll: None,
            focused: true,
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            alternate_screen_active: false,
            pixel_width: 0,
            pixel_height: 0,
        }],
        splits: Vec::new(),
        popup: None,
        graphics: Default::default(),
    }
}

fn machine() -> PendingEndpointActivation {
    PendingEndpointActivation {
        source: lease(ClientEndpointId::Local, 1, "local-boot"),
        source_background: None,
        source_available: true,
        target: lease(endpoint(), 7, "remote-boot"),
        focus: None,
        host_focused: true,
        resize: resize(),
        phase: ActivationPhase::ActivatingTarget {
            request_id: "client-shell-surface:3:on".into(),
            acknowledged_revision: Some(1),
            focus_request_id: None,
            focus_request_target: None,
            focus_acknowledged: true,
            evidence: ActivationEvidence::default(),
        },
        deadline: Instant::now() + ACTIVATION_TIMEOUT,
        epoch: 3,
        next_focus_serial: 0,
        rollback_error: None,
        successor: None,
        input: BufferedInput::default(),
    }
}

#[test]
fn source_release_and_target_activation_leave_in_one_write_batch() {
    let (shell, mut endpoints, local_sent, remote_sent) = shell_and_registry();
    let mut activation = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        endpoint(),
        None,
        resize(),
        11,
        Instant::now(),
    )
    .unwrap();
    let local = local_sent.lock().unwrap();
    assert_eq!(
        local.first(),
        Some(&crate::protocol::ClientMessage::ClientShellFocus { focused: false }),
        "source focus is revoked before source-off"
    );
    assert_eq!(
        local
            .iter()
            .filter_map(surface_set_active)
            .collect::<Vec<_>>(),
        vec![false]
    );
    drop(local);
    let remote = remote_sent.lock().unwrap();
    assert_eq!(
        remote[0],
        resize(),
        "the target starts without the source reply"
    );
    assert_eq!(remote.get(1).and_then(surface_set_active), Some(true));
    assert_eq!(
        remote.get(2),
        Some(&crate::protocol::ClientMessage::ClientShellFocus { focused: true })
    );
    drop(remote);
    assert!(
        !endpoints.active_surface_available(),
        "pane input is blocked while frozen"
    );
    assert_eq!(
        activation.receive_response(
            &ClientEndpointId::Local,
            1,
            "client-shell-surface:11:off",
            &surface_success("client-shell-surface:11:off", false, 1),
            &mut endpoints,
        ),
        SurfaceActivationProgress::Stale,
        "nothing waits on the source release"
    );
}

#[test]
fn a_failed_source_release_write_does_not_hold_back_the_target() {
    let (shell, mut endpoints, _local_sent, remote_sent) =
        shell_and_registry_with_failures(true, false);
    let activation = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        endpoint(),
        None,
        resize(),
        12,
        Instant::now(),
    )
    .unwrap();
    assert!(matches!(
        activation.phase,
        ActivationPhase::ActivatingTarget { .. }
    ));
    assert_eq!(
        remote_sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(surface_set_active)
            .collect::<Vec<_>>(),
        vec![true]
    );
}

#[test]
fn active_source_requires_metadata_from_its_current_connection_generation() {
    let (mut shell, mut endpoints, local_sent, remote_sent) = shell_and_registry();
    shell.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        99,
        Box::new(test_snapshot("stale-local-boot", 1)),
    );

    let result = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        endpoint(),
        None,
        resize(),
        26,
        Instant::now(),
    );

    assert!(
        matches!(result, Err(ActivationBeginError::Preflight(message)) if message.contains("this connection"))
    );
    assert!(local_sent.lock().unwrap().is_empty());
    assert!(remote_sent.lock().unwrap().is_empty());
    assert!(endpoints.active_surface_available());
}

#[test]
fn observed_begin_write_failure_returns_recoverable_partial_activation() {
    let (shell, mut endpoints, local_sent, remote_sent) =
        shell_and_registry_with_failures(false, true);
    let result = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        endpoint(),
        None,
        resize(),
        10,
        Instant::now(),
    );

    let ActivationBeginError::Partial {
        mut activation,
        error,
    } = (match result {
        Err(error) => error,
        Ok(_) => panic!("an observed target write must return partial lifecycle state"),
    })
    else {
        panic!("an observed target write must return partial lifecycle state");
    };
    assert!(error.contains("resize"));
    assert_eq!(
        local_sent.lock().unwrap().first(),
        Some(&crate::protocol::ClientMessage::ClientShellFocus { focused: false })
    );
    assert_eq!(remote_sent.lock().unwrap().as_slice(), &[resize()]);
    assert!(matches!(
        activation.rollback(&mut endpoints, error),
        ActivationRollback::Unavailable(_)
    ));
}

#[test]
fn activation_requires_an_exact_snapshot_surface_revision_pair() {
    let mut activation = machine();
    let target = endpoint();
    let snapshot = crate::protocol::ClientShellSnapshot {
        boot_id: "remote-boot".into(),
        revision: 2,
        config_diagnostic: None,
        product_announcement: None,
        update_available: None,
        update_install_command: String::new(),
        server_keybindings_toml: None,
        latest_release_notes_available: false,
        integration_updates_available: false,
        worktree_directory: String::new(),
        release_notes: None,
        focused_workspace_id: None,
        focused_tab_id: None,
        focused_pane_id: None,
        tab_bar_right: Vec::new(),
        tab_bar_right_separator: String::new(),
        agent_view_label: None,
        agent_order: Vec::new(),
        workspaces: Vec::new(),
        tabs: Vec::new(),
        panes: Vec::new(),
        agents: Vec::new(),
        commands: Vec::new(),
        pane_focus_style: None,
    };
    assert_eq!(
        activation.receive_snapshot(&target, 7, &snapshot),
        SurfaceActivationProgress::Pending
    );
    assert_eq!(
        activation.receive_surface(&target, 7, surface("remote-boot", 1, "pane")),
        SurfaceActivationProgress::Pending
    );
    assert_eq!(
        activation.receive_surface(&target, 7, surface("remote-boot", 2, "pane")),
        SurfaceActivationProgress::Ready
    );
}

#[test]
fn typed_target_ack_sets_a_floor_for_same_boot_activation_evidence() {
    let (shell, mut endpoints, _local_sent, _remote_sent) = shell_and_registry();
    let target = endpoint();
    let mut activation = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        target.clone(),
        None,
        resize(),
        16,
        Instant::now(),
    )
    .unwrap();
    let _ = activation.receive_response(
        &ClientEndpointId::Local,
        1,
        "client-shell-surface:16:off",
        &surface_success("client-shell-surface:16:off", false, 1),
        &mut endpoints,
    );
    assert_eq!(
        activation.receive_response(
            &target,
            7,
            "client-shell-surface:16:on",
            &surface_success("client-shell-surface:16:on", true, 4),
            &mut endpoints,
        ),
        SurfaceActivationProgress::Pending
    );
    assert_eq!(
        activation.receive_snapshot(&target, 7, &test_snapshot("remote-boot", 3)),
        SurfaceActivationProgress::Pending
    );
    assert_eq!(
        activation.receive_surface(&target, 7, surface("remote-boot", 3, "pane")),
        SurfaceActivationProgress::Pending,
        "a delayed same-boot surface below the acknowledgement floor is not evidence"
    );
    assert_eq!(
        activation.receive_snapshot(&target, 7, &test_snapshot("remote-boot", 4)),
        SurfaceActivationProgress::Pending
    );
    assert_eq!(
        activation.receive_surface(&target, 7, surface("remote-boot", 4, "pane")),
        SurfaceActivationProgress::Ready
    );
}

fn cell(symbol: &str) -> crate::protocol::CellData {
    crate::protocol::CellData {
        symbol: symbol.into(),
        fg: 0,
        bg: 0,
        modifier: 0,
        skip: false,
        hyperlink: None,
    }
}

fn surface_with_cells(boot_id: &str, revision: u64) -> crate::protocol::PaneSurfaceFrame {
    let mut surface = surface(boot_id, revision, "pane");
    surface.frame.cells = vec![cell(" "); 80 * 24];
    surface
}

fn patch_cell(
    surface: &crate::protocol::PaneSurfaceFrame,
    base_surface_revision: u64,
    x: u16,
    symbol: &str,
) -> crate::protocol::PaneSurfacePatch {
    crate::protocol::PaneSurfacePatch {
        boot_id: surface.boot_id.clone(),
        projection_revision: surface.projection_revision,
        base_surface_revision,
        surface_revision: base_surface_revision + 1,
        rows: vec![crate::protocol::PaneSurfacePatchRow {
            x,
            y: 0,
            cells: vec![cell(symbol)],
        }],
        panes: surface.panes.clone(),
        cursor: None,
    }
}

#[test]
fn target_patches_before_the_focus_reply_reach_the_committed_frame() {
    let (mut shell, mut endpoints, _local_sent, _remote_sent) = shell_and_registry();
    let target = endpoint();
    let mut activation = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        target.clone(),
        Some(crate::client::shell::ClientEndpointFocusTarget::Workspace(
            "ws".into(),
        )),
        resize(),
        40,
        Instant::now(),
    )
    .unwrap();
    let _ = activation.receive_response(
        &ClientEndpointId::Local,
        1,
        "client-shell-surface:40:off",
        &surface_success("client-shell-surface:40:off", false, 1),
        &mut endpoints,
    );
    assert_eq!(
        activation.receive_response(
            &target,
            7,
            "client-shell-surface:40:on",
            &surface_success("client-shell-surface:40:on", true, 2),
            &mut endpoints,
        ),
        SurfaceActivationProgress::Pending
    );
    let mut snapshot = test_snapshot("remote-boot", 2);
    snapshot.focused_workspace_id = Some("ws".into());
    shell.cache_endpoint_snapshot_inactive_for_generation(&target, 7, Box::new(snapshot.clone()));
    assert_eq!(
        activation.receive_snapshot(&target, 7, &snapshot),
        SurfaceActivationProgress::Pending
    );
    let full = surface_with_cells("remote-boot", 2);
    assert_eq!(
        activation.receive_surface(&target, 7, full.clone()),
        SurfaceActivationProgress::Pending,
        "the focus reply is still outstanding"
    );

    // The endpoint keeps streaming output while its focus reply waits on a response thread. Each
    // patch is already the endpoint's baseline once it is sent.
    assert!(activation.receive_surface_patch(&target, 7, &patch_cell(&full, 2, 0, "a")));
    assert!(activation.receive_surface_patch(&target, 7, &patch_cell(&full, 3, 1, "b")));
    assert!(
        !activation.receive_surface_patch(&target, 6, &patch_cell(&full, 4, 2, "x")),
        "a stale connection cannot advance the collected surface"
    );
    assert_eq!(
        activation.receive_response(
            &target,
            7,
            "client-shell-focus:40:1",
            &workspace_focus_success("client-shell-focus:40:1", "ws"),
            &mut endpoints,
        ),
        SurfaceActivationProgress::Ready
    );
    let ActivationPhase::ActivatingTarget { evidence, .. } = &activation.phase else {
        panic!("activation is still collecting target evidence");
    };
    let collected = evidence.surface.as_ref().expect("collected surface");
    assert_eq!(collected.surface_revision, 4);
    assert_eq!(collected.frame.cells[0].symbol, "a");
    assert_eq!(collected.frame.cells[1].symbol, "b");
    assert_eq!(collected.frame.cells[2].symbol, " ");

    assert!(matches!(
        activation.complete(&mut shell, &mut endpoints),
        Ok(CommittedActivation {
            completion: ActivationCompletion::Activated,
            ..
        })
    ));
    // The committed frame alone must already match the endpoint baseline, so the next live
    // patch applies.
    assert!(matches!(
        shell.apply_pane_surface_patch(patch_cell(&full, 4, 2, "c")),
        crate::client::shell::ClientPaneSurfacePatchOutcome::Applied(_)
    ));
}

#[test]
fn stale_generation_and_boot_are_not_activation_evidence() {
    let mut activation = machine();
    assert_eq!(
        activation.receive_surface(&endpoint(), 6, surface("remote-boot", 1, "pane")),
        SurfaceActivationProgress::Stale
    );
    assert_eq!(
        activation.receive_surface(&endpoint(), 7, surface("old-boot", 1, "pane")),
        SurfaceActivationProgress::Stale
    );
}

#[test]
fn stale_response_boot_is_not_consumed() {
    let mut activation = machine();
    let (_shell, mut endpoints, _local_sent, _remote_sent) = shell_and_registry();
    assert_eq!(
        activation.receive_response_for_boot(
            &endpoint(),
            7,
            "old-boot",
            "client-shell-surface:3:on",
            &surface_success("client-shell-surface:3:on", true, 2),
            &mut endpoints,
        ),
        SurfaceActivationProgress::Stale
    );
}

#[test]
fn same_target_retarget_is_latest_wins() {
    let (shell, mut endpoints, _local_sent, remote_sent) = shell_and_registry();
    let target = endpoint();
    let mut activation = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        target.clone(),
        Some(crate::client::shell::ClientEndpointFocusTarget::Workspace(
            "old".into(),
        )),
        resize(),
        12,
        Instant::now(),
    )
    .unwrap();
    let _ = activation.receive_response(
        &ClientEndpointId::Local,
        1,
        "client-shell-surface:12:off",
        &surface_success("client-shell-surface:12:off", false, 1),
        &mut endpoints,
    );
    let old_focus = remote_sent
        .lock()
        .unwrap()
        .iter()
        .find_map(|message| match message {
            crate::protocol::ClientMessage::ClientShellEndpointRequest { request, .. }
                if surface_set_active(message).is_none() =>
            {
                Some(
                    serde_json::from_str::<crate::api::schema::Request>(request)
                        .unwrap()
                        .id,
                )
            }
            _ => None,
        })
        .unwrap();
    let sent_before_retarget = remote_sent.lock().unwrap().len();
    activation
        .retarget(
            Some(crate::client::shell::ClientEndpointFocusTarget::Workspace(
                "new".into(),
            )),
            &mut endpoints,
        )
        .unwrap();
    assert_eq!(remote_sent.lock().unwrap().len(), sent_before_retarget);
    assert!(activation.accepts_response(&target, 7, "remote-boot", &old_focus));
    assert_eq!(
        activation.receive_response(
            &target,
            7,
            &old_focus,
            &workspace_focus_success(&old_focus, "old"),
            &mut endpoints,
        ),
        SurfaceActivationProgress::Pending
    );
    let latest_focus = remote_sent
        .lock()
        .unwrap()
        .last()
        .and_then(|message| match message {
            crate::protocol::ClientMessage::ClientShellEndpointRequest { request, .. } => Some(
                serde_json::from_str::<crate::api::schema::Request>(request)
                    .unwrap()
                    .id,
            ),
            _ => None,
        })
        .unwrap();
    assert_ne!(latest_focus, old_focus);
    assert!(activation.accepts_response(&target, 7, "remote-boot", &latest_focus));
}

#[test]
fn latest_host_focus_reaches_the_pending_target() {
    let (shell, mut endpoints, _local_sent, remote_sent) = shell_and_registry();
    let mut activation = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        endpoint(),
        None,
        resize(),
        25,
        Instant::now(),
    )
    .unwrap();
    assert_eq!(
        remote_sent.lock().unwrap().get(2),
        Some(&crate::protocol::ClientMessage::ClientShellFocus { focused: true }),
        "the activation carries the host focus baseline"
    );

    for focused in [false, true] {
        activation
            .update_host_focus(focused, &mut endpoints)
            .unwrap();
        assert_eq!(
            remote_sent.lock().unwrap().last(),
            Some(&crate::protocol::ClientMessage::ClientShellFocus { focused })
        );
    }
}

#[test]
fn host_focus_change_keeps_the_collected_surface() {
    let (_shell, mut endpoints, _local_sent, remote_sent) = shell_and_registry();
    let mut activation = machine();
    let _ = activation.receive_snapshot(&endpoint(), 7, &test_snapshot("remote-boot", 1));
    assert_eq!(
        activation.receive_surface(&endpoint(), 7, surface("remote-boot", 1, "pane")),
        SurfaceActivationProgress::Ready
    );

    activation.update_host_focus(false, &mut endpoints).unwrap();

    assert_eq!(
        remote_sent.lock().unwrap().as_slice(),
        &[crate::protocol::ClientMessage::ClientShellFocus { focused: false }]
    );
    assert_eq!(
        activation.progress(),
        SurfaceActivationProgress::Ready,
        "the repaint it causes arrives as patches on the collected surface"
    );
}

#[test]
fn resize_invalidates_already_recorded_surface_evidence() {
    let (_shell, mut endpoints, _local_sent, _remote_sent) = shell_and_registry();
    let mut activation = machine();
    assert_eq!(
        activation.receive_snapshot(&endpoint(), 7, &test_snapshot("remote-boot", 1)),
        SurfaceActivationProgress::Pending
    );
    assert_eq!(
        activation.receive_surface(&endpoint(), 7, surface("remote-boot", 1, "pane")),
        SurfaceActivationProgress::Ready
    );
    let resize = crate::protocol::ClientMessage::ClientShellResize {
        cell_width_px: 9,
        cell_height_px: 17,
        surface_size: crate::protocol::ClientSurfaceSize {
            cols: 100,
            rows: 30,
        },
        pixel_mouse: true,
    };
    activation.update_resize(resize, &mut endpoints).unwrap();
    assert_eq!(activation.progress(), SurfaceActivationProgress::Pending);
}

#[test]
fn resize_during_activation_reaches_the_pending_target() {
    let (shell, mut endpoints, _local_sent, remote_sent) = shell_and_registry();
    let mut activation = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        endpoint(),
        None,
        resize(),
        15,
        Instant::now(),
    )
    .unwrap();
    let _ = activation.receive_response(
        &ClientEndpointId::Local,
        1,
        "client-shell-surface:15:off",
        &surface_success("client-shell-surface:15:off", false, 1),
        &mut endpoints,
    );
    let resized = crate::protocol::ClientMessage::ClientShellResize {
        cell_width_px: 9,
        cell_height_px: 17,
        surface_size: crate::protocol::ClientSurfaceSize {
            cols: 100,
            rows: 30,
        },
        pixel_mouse: true,
    };
    activation
        .update_resize(resized.clone(), &mut endpoints)
        .unwrap();
    assert_eq!(remote_sent.lock().unwrap().last(), Some(&resized));
    assert_eq!(
        activation.receive_surface(&endpoint(), 7, surface("remote-boot", 1, "pane")),
        SurfaceActivationProgress::Pending,
        "a surface for the prior geometry cannot commit"
    );
}

#[test]
fn rapid_a_to_b_to_a_restores_source_before_a_fresh_latest_epoch() {
    let (mut shell, mut endpoints, local_sent, remote_sent) = shell_and_registry();
    let mut activation = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        endpoint(),
        None,
        resize(),
        20,
        Instant::now(),
    )
    .unwrap();
    let _ = activation.receive_response(
        &ClientEndpointId::Local,
        1,
        "client-shell-surface:20:off",
        &surface_success("client-shell-surface:20:off", false, 1),
        &mut endpoints,
    );

    // The latest A-qualified target replaces B while B may already have accepted target-on.
    assert_eq!(
        activation.supersede(
            ClientEndpointId::Local,
            Some(crate::client::shell::ClientEndpointFocusTarget::Pane(
                "local-pane".into(),
            )),
            &mut endpoints,
        ),
        ActivationRollback::Pending
    );
    assert_eq!(
        remote_sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(surface_set_active)
            .collect::<Vec<_>>(),
        vec![true, false],
        "B is released before A can be restored"
    );
    assert_eq!(
        activation.receive_surface(&endpoint(), 7, surface("remote-boot", 2, "pane")),
        SurfaceActivationProgress::Stale,
        "delayed B activation evidence cannot satisfy A restoration"
    );
    let _ = activation.receive_response(
        &endpoint(),
        7,
        "client-shell-surface:20:rollback-target-off",
        &surface_success("client-shell-surface:20:rollback-target-off", false, 1),
        &mut endpoints,
    );
    assert_eq!(
        local_sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(surface_set_active)
            .collect::<Vec<_>>(),
        vec![false, true],
        "source restoration is acknowledged rather than racing target ownership"
    );
    let _ = activation.receive_response(
        &ClientEndpointId::Local,
        1,
        "client-shell-surface:20:rollback-source-on",
        &surface_success("client-shell-surface:20:rollback-source-on", true, 2),
        &mut endpoints,
    );
    let local_snapshot = test_snapshot("local-boot", 2);
    shell.set_snapshot(Box::new(local_snapshot.clone()));
    assert_eq!(
        activation.receive_snapshot(&ClientEndpointId::Local, 1, &local_snapshot),
        SurfaceActivationProgress::Pending
    );
    assert_eq!(
        activation.receive_surface(
            &ClientEndpointId::Local,
            1,
            surface("local-boot", 2, "pane")
        ),
        SurfaceActivationProgress::Ready
    );
    assert!(matches!(
        activation.complete(&mut shell, &mut endpoints),
        Ok(CommittedActivation {
            completion: ActivationCompletion::RestoredSource {
                successor: Some(EndpointActivationIntent {
                    endpoint_id: ClientEndpointId::Local,
                    ..
                }),
                ..
            },
            ..
        })
    ));
    assert_eq!(endpoints.active_id(), &ClientEndpointId::Local);

    // Runtime queues this successor with force=true, so even source==target receives a new
    // activation epoch only after restoration committed.
    let _fresh_epoch = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        ClientEndpointId::Local,
        Some(crate::client::shell::ClientEndpointFocusTarget::Pane(
            "local-pane".into(),
        )),
        resize(),
        21,
        Instant::now(),
    )
    .unwrap();
    assert!(local_sent.lock().unwrap().iter().any(|message| {
        matches!(
            message,
            crate::protocol::ClientMessage::ClientShellEndpointRequest { request, .. }
                if serde_json::from_str::<crate::api::schema::Request>(request)
                    .is_ok_and(|request| request.id == "client-shell-surface:21:on")
        )
    }));
}

#[test]
fn local_activation_does_not_wait_for_a_disconnected_stalled_or_failed_remote() {
    for source_state in ["disconnected", "stalled", "write-failed"] {
        local_escape(source_state);
    }
}

fn local_escape(source_state: &str) {
    let (mut shell, mut endpoints, local_sent, remote_sent) = shell_and_registry();
    let disconnected = endpoint();
    endpoints.set_surface_active(&ClientEndpointId::Local, false);
    endpoints.set_surface_active(&disconnected, true);
    assert!(endpoints.set_active(&disconnected));
    assert!(shell.activate_endpoint_projection(&disconnected));
    match source_state {
        "disconnected" => endpoints.disconnect(&disconnected),
        "write-failed" => endpoints.insert(
            disconnected.clone(),
            FakeTransport {
                sent: remote_sent,
                fail_after_write: true,
            },
            7,
            negotiation(),
            true,
        ),
        _ => {}
    }

    let mut activation = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        ClientEndpointId::Local,
        None,
        resize(),
        22,
        Instant::now(),
    )
    .unwrap();
    let local_activations = local_sent
        .lock()
        .unwrap()
        .iter()
        .filter_map(surface_set_active)
        .collect::<Vec<_>>();
    assert_eq!(local_activations, vec![true], "Local must not wait for SSH");
    assert!(!endpoints.active_surface_available());
    assert_eq!(endpoints.active_id(), &disconnected);
    assert_eq!(
        activation.receive_response(
            &disconnected,
            7,
            "client-shell-surface:22:off",
            &surface_success("client-shell-surface:22:off", false, 1),
            &mut endpoints,
        ),
        SurfaceActivationProgress::Stale
    );
    assert_eq!(
        activation.receive_surface(&disconnected, 7, surface("remote-boot", 2, "stale")),
        SurfaceActivationProgress::Stale
    );

    assert_eq!(
        activation.receive_response(
            &ClientEndpointId::Local,
            1,
            "client-shell-surface:22:on",
            &surface_success("client-shell-surface:22:on", true, 2),
            &mut endpoints,
        ),
        SurfaceActivationProgress::Pending
    );
    let snapshot = test_snapshot("local-boot", 2);
    shell.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        1,
        Box::new(snapshot.clone()),
    );
    assert_eq!(
        activation.receive_snapshot(&ClientEndpointId::Local, 1, &snapshot),
        SurfaceActivationProgress::Pending
    );
    assert_eq!(
        activation.receive_surface(
            &ClientEndpointId::Local,
            1,
            surface("local-boot", 2, "pane")
        ),
        SurfaceActivationProgress::Ready
    );
    assert_eq!(
        activation
            .complete(&mut shell, &mut endpoints)
            .map(|committed| committed.completion),
        Ok(ActivationCompletion::Activated)
    );
    assert_eq!(endpoints.active_id(), &ClientEndpointId::Local);
    assert_ne!(endpoints.active_id(), &disconnected);
    assert!(
        !endpoints.active_surface_available(),
        "the runtime opens input once it has presented the committed frame"
    );
}

#[test]
fn local_selection_abandons_every_unfinished_remote_handoff_phase() {
    use crate::client::{
        endpoint_commands::EndpointCommands, shell_runtime::begin_endpoint_activation, ClientState,
    };
    for phase in ["target", "rollback", "restore"] {
        let (shell, mut endpoints, local_sent, _remote_sent) = shell_and_registry();
        let mut abandoned = PendingEndpointActivation::begin(
            &shell,
            &mut endpoints,
            endpoint(),
            None,
            resize(),
            30,
            Instant::now(),
        )
        .unwrap();
        if matches!(phase, "rollback" | "restore") {
            assert_eq!(
                abandoned.rollback(&mut endpoints, "cancel".into()),
                ActivationRollback::Pending
            );
        }
        if phase == "restore" {
            abandoned.receive_response(
                &endpoint(),
                7,
                "client-shell-surface:30:rollback-target-off",
                &surface_success("client-shell-surface:30:rollback-target-off", false, 1),
                &mut endpoints,
            );
        }
        local_sent.lock().unwrap().clear();
        let mut state = ClientState::test_new();
        state.shell = Some(shell);
        let mut commands = EndpointCommands::default();
        let mut pending = Some(abandoned);
        let mut serial = 31;
        let mut scheduled = None;
        for _ in 0..2 {
            begin_endpoint_activation(
                &mut state,
                &mut endpoints,
                &mut commands,
                &mut pending,
                &mut serial,
                ClientEndpointId::Local,
                None,
                false,
                Instant::now(),
                &mut scheduled,
            )
            .unwrap();
        }
        assert_eq!(serial, 32, "repeated Local selection must coalesce");
        let local = pending.as_mut().unwrap();
        assert_eq!(local.target(), &ClientEndpointId::Local);
        assert!(!endpoints.active_surface_available());
        assert!(state.presentation_frozen);
        let activations = local_sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(surface_set_active)
            .collect::<Vec<_>>();
        assert_eq!(activations, vec![false, true], "phase: {phase}");
        assert!(!local.accepts_response(
            &ClientEndpointId::Local,
            1,
            "local-boot",
            "client-shell-surface:30:off"
        ));
        assert_eq!(
            local.receive_surface(&endpoint(), 7, surface("remote-boot", 2, "stale")),
            SurfaceActivationProgress::Stale
        );
    }
}

#[test]
fn local_selection_waits_for_fresh_metadata_without_abandoning_remote() {
    use crate::client::{
        endpoint_commands::EndpointCommands,
        shell_runtime::{begin_endpoint_activation, take_ready_local_activation},
        ClientLoopEvent, ClientState,
    };
    for replaced_generation in [false, true] {
        let (mut shell, mut endpoints, local_sent, remote_sent) = shell_and_registry();
        shell.set_endpoint_snapshot_for_generation(
            &ClientEndpointId::Local,
            1,
            Box::new(test_snapshot("local-boot", 1)),
        );
        let mut state = ClientState::test_new();
        state.shell = Some(shell);
        let mut commands = EndpointCommands::default();
        let mut pending = None;
        let mut serial = 40;
        let mut scheduled = None;
        begin_endpoint_activation(
            &mut state,
            &mut endpoints,
            &mut commands,
            &mut pending,
            &mut serial,
            endpoint(),
            None,
            false,
            Instant::now(),
            &mut scheduled,
        )
        .unwrap();
        if replaced_generation {
            endpoints.insert(
                ClientEndpointId::Local,
                FakeTransport {
                    sent: local_sent.clone(),
                    fail_after_write: false,
                },
                2,
                negotiation(),
                false,
            );
        } else {
            endpoints.disconnect(&ClientEndpointId::Local);
        }
        begin_endpoint_activation(
            &mut state,
            &mut endpoints,
            &mut commands,
            &mut pending,
            &mut serial,
            ClientEndpointId::Local,
            Some(crate::client::shell::ClientEndpointFocusTarget::Workspace(
                "selected-local".into(),
            )),
            false,
            Instant::now(),
            &mut scheduled,
        )
        .unwrap();
        assert_eq!(
            pending.as_ref().map(PendingEndpointActivation::target),
            Some(&endpoint())
        );
        assert_eq!(
            remote_sent
                .lock()
                .unwrap()
                .iter()
                .filter_map(surface_set_active)
                .collect::<Vec<_>>(),
            vec![true],
            "the remote activation is still in flight"
        );
        assert_eq!(serial, 41);
        assert!(state.deferred_local_activation.is_some());
        assert!(take_ready_local_activation(&mut state, &endpoints).is_none());
        if !replaced_generation {
            endpoints.insert(
                ClientEndpointId::Local,
                FakeTransport {
                    sent: local_sent,
                    fail_after_write: false,
                },
                2,
                negotiation(),
                false,
            );
        }
        assert!(take_ready_local_activation(&mut state, &endpoints).is_none());
        state
            .shell
            .as_mut()
            .unwrap()
            .cache_endpoint_snapshot_inactive_for_generation(
                &ClientEndpointId::Local,
                2,
                Box::new(test_snapshot("local-boot", 1)),
            );
        let event = take_ready_local_activation(&mut state, &endpoints).unwrap();
        let ClientLoopEvent::ActivateEndpoint {
            endpoint_id,
            target,
            force,
        } = event
        else {
            panic!("expected retained Local selection");
        };
        assert_eq!(endpoint_id, ClientEndpointId::Local);
        assert_eq!(
            target,
            Some(crate::client::shell::ClientEndpointFocusTarget::Workspace(
                "selected-local".into()
            ))
        );
        begin_endpoint_activation(
            &mut state,
            &mut endpoints,
            &mut commands,
            &mut pending,
            &mut serial,
            endpoint_id,
            target,
            force,
            Instant::now(),
            &mut scheduled,
        )
        .unwrap();
        assert_eq!(pending.as_ref().unwrap().target(), &ClientEndpointId::Local);
        assert_eq!(serial, 42);
        assert!(!endpoints.active_surface_available());
        assert!(state.deferred_local_activation.is_none());
    }
}

#[test]
fn newer_remote_selection_cancels_deferred_local_selection() {
    use crate::client::{
        endpoint_commands::EndpointCommands, shell_runtime::begin_endpoint_activation, ClientState,
    };
    let (shell, mut endpoints, _, _) = shell_and_registry();
    let mut state = ClientState::test_new();
    state.shell = Some(shell);
    let mut commands = EndpointCommands::default();
    let mut pending = None;
    let mut serial = 50;
    let mut scheduled = None;
    endpoints.disconnect(&ClientEndpointId::Local);
    for endpoint_id in [ClientEndpointId::Local, endpoint()] {
        begin_endpoint_activation(
            &mut state,
            &mut endpoints,
            &mut commands,
            &mut pending,
            &mut serial,
            endpoint_id.clone(),
            None,
            false,
            Instant::now(),
            &mut scheduled,
        )
        .unwrap();
        assert_eq!(
            state.deferred_local_activation.is_some(),
            endpoint_id.is_local()
        );
    }
    assert_eq!(pending.as_ref().unwrap().target(), &endpoint());
}

#[test]
fn rollback_keeps_the_latest_intent_even_when_it_returns_to_the_target() {
    let (shell, mut endpoints, _local_sent, _remote_sent) = shell_and_registry();
    let target = endpoint();
    let mut activation = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        target.clone(),
        None,
        resize(),
        23,
        Instant::now(),
    )
    .unwrap();
    let _ = activation.receive_response(
        &ClientEndpointId::Local,
        1,
        "client-shell-surface:23:off",
        &surface_success("client-shell-surface:23:off", false, 1),
        &mut endpoints,
    );
    assert_eq!(
        activation.supersede(
            ClientEndpointId::Local,
            Some(crate::client::shell::ClientEndpointFocusTarget::Pane(
                "local-pane".into()
            )),
            &mut endpoints,
        ),
        ActivationRollback::Pending
    );
    assert!(!activation.can_retarget(&target));
    assert_eq!(
        activation.supersede(
            target.clone(),
            Some(crate::client::shell::ClientEndpointFocusTarget::Pane(
                "remote-pane".into()
            )),
            &mut endpoints,
        ),
        ActivationRollback::Pending
    );
    assert_eq!(
        activation.successor,
        Some(EndpointActivationIntent {
            endpoint_id: target,
            target: Some(crate::client::shell::ClientEndpointFocusTarget::Pane(
                "remote-pane".into()
            )),
        })
    );
}

#[test]
fn unacknowledged_target_release_closes_target_before_restoring_source() {
    let (shell, mut endpoints, local_sent, _remote_sent) = shell_and_registry();
    let target = endpoint();
    let mut activation = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        target.clone(),
        None,
        resize(),
        24,
        Instant::now(),
    )
    .unwrap();
    let _ = activation.receive_response(
        &ClientEndpointId::Local,
        1,
        "client-shell-surface:24:off",
        &surface_success("client-shell-surface:24:off", false, 1),
        &mut endpoints,
    );
    assert_eq!(
        activation.rollback(&mut endpoints, "target activation timed out".into()),
        ActivationRollback::Pending
    );
    assert_eq!(
        activation.rollback(&mut endpoints, "target release timed out".into()),
        ActivationRollback::Pending
    );
    assert!(endpoints.connection(&target).is_none());
    let failures = endpoints.take_failures();
    assert_eq!(
        failures.len(),
        1,
        "rollback revocation must reach the reconnect owner"
    );
    assert_eq!(failures[0].endpoint_id, target);
    assert_eq!(failures[0].generation, 7);
    assert_eq!(failures[0].kind, std::io::ErrorKind::TimedOut);
    assert_eq!(
        local_sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(surface_set_active)
            .collect::<Vec<_>>(),
        vec![false, true]
    );
}

#[test]
fn target_loss_at_activation_deadline_restores_source_before_timeout() {
    let (shell, mut endpoints, local_sent, _) = shell_and_registry();
    let target = endpoint();
    let mut activation = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        target.clone(),
        None,
        resize(),
        30,
        Instant::now(),
    )
    .unwrap();
    activation.receive_response(
        &ClientEndpointId::Local,
        1,
        "client-shell-surface:30:off",
        &surface_success("client-shell-surface:30:off", false, 1),
        &mut endpoints,
    );
    let now = Instant::now();
    activation.deadline = now;
    assert!(activation.expired(now));
    endpoints.fail(&target, std::io::ErrorKind::UnexpectedEof.into());
    // Match the client timer: apply transport failures before checking phase expiry.
    for failure in endpoints.take_failures() {
        assert_eq!(
            activation.endpoint_disconnected(&mut endpoints, &failure.endpoint_id, failure.message),
            ActivationRollback::Pending
        );
    }
    assert!(matches!(
        activation.phase,
        ActivationPhase::RestoringSource { .. }
    ));
    assert!(!activation.expired(now));
    assert_eq!(
        local_sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(surface_set_active)
            .collect::<Vec<_>>(),
        vec![false, true]
    );
}

#[test]
fn losing_local_during_handoff_does_not_revoke_the_healthy_target() {
    let (shell, mut endpoints, _local_sent, remote_sent) = shell_and_registry();
    let target = endpoint();
    let mut activation = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        target.clone(),
        None,
        resize(),
        29,
        Instant::now(),
    )
    .unwrap();
    endpoints.fail(
        &ClientEndpointId::Local,
        std::io::ErrorKind::BrokenPipe.into(),
    );
    assert_eq!(
        activation.endpoint_disconnected(
            &mut endpoints,
            &ClientEndpointId::Local,
            "Local stopped".into()
        ),
        ActivationRollback::Pending
    );
    assert!(!activation.source_available);
    assert!(matches!(
        activation.phase,
        ActivationPhase::ActivatingTarget { .. }
    ));
    assert!(endpoints.connection(&target).is_some());
    assert_eq!(
        remote_sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(surface_set_active)
            .collect::<Vec<_>>(),
        vec![true]
    );
}

#[test]
fn resize_message_preserves_the_latest_surface_dimensions() {
    assert_eq!(
        resize_geometry(&resize()),
        Some(crate::protocol::ClientSurfaceSize { cols: 80, rows: 24 })
    );
}

fn mouse_capture(enabled: bool) -> crate::protocol::ServerMessage {
    crate::protocol::ServerMessage::MouseCapture {
        enabled,
        sgr_pixels: false,
    }
}

fn scroll() -> crate::protocol::ClientPaneInputEvent {
    crate::protocol::ClientPaneInputEvent::Mouse {
        kind: crate::protocol::ClientMouseKind::ScrollUp,
        position: crate::protocol::ClientMousePosition::Cell { column: 1, row: 1 },
        geometry: None,
        modifiers: 0,
        lines: 1,
    }
}

fn text(value: &str) -> crate::protocol::ClientPaneInputEvent {
    crate::protocol::ClientPaneInputEvent::TextCommit(value.into())
}

#[test]
fn held_effects_keep_the_latest_modes_and_sum_bells() {
    use crate::protocol::ServerMessage;

    let mut evidence = ActivationEvidence::default();
    evidence.record_effect(mouse_capture(false));
    evidence.record_effect(ServerMessage::TerminalBell { count: 2 });
    evidence.record_effect(ServerMessage::TerminalBell { count: 3 });
    evidence.record_effect(ServerMessage::WindowTitle {
        title: Some("old".into()),
    });
    evidence.record_effect(mouse_capture(true));
    evidence.record_effect(ServerMessage::WindowTitle {
        title: Some("new".into()),
    });

    assert_eq!(
        evidence.effects,
        vec![
            ServerMessage::TerminalBell { count: 5 },
            mouse_capture(true),
            ServerMessage::WindowTitle {
                title: Some("new".into())
            },
        ]
    );
}

#[test]
fn buffered_input_drops_mouse_and_all_of_an_overflowing_switch() {
    let mut input = BufferedInput::default();
    input.push(vec![text("a"), scroll(), text("b")]);
    assert_eq!(input.take(), vec![text("a"), text("b")]);

    input.push(vec![text("kept")]);
    input.push(vec![crate::protocol::ClientPaneInputEvent::Paste(
        "x".repeat(1 << 20),
    )]);
    input.push(vec![text("after overflow")]);
    assert!(
        input.take().is_empty(),
        "a prefix of what was typed is never delivered"
    );
}

#[test]
fn target_effects_wait_for_the_commit_and_source_effects_are_dropped() {
    let (mut shell, mut endpoints, _local_sent, _remote_sent) = shell_and_registry();
    let target = endpoint();
    let mut activation = machine();
    activation.receive_presentation_effect(&ClientEndpointId::Local, 1, mouse_capture(false));
    activation.receive_presentation_effect(&target, 6, mouse_capture(false));
    activation.receive_presentation_effect(&target, 7, mouse_capture(true));

    let snapshot = test_snapshot("remote-boot", 1);
    shell.cache_endpoint_snapshot_inactive_for_generation(&target, 7, Box::new(snapshot.clone()));
    let _ = activation.receive_snapshot(&target, 7, &snapshot);
    assert_eq!(
        activation.receive_surface(&target, 7, surface("remote-boot", 1, "pane")),
        SurfaceActivationProgress::Ready
    );
    let committed = activation.complete(&mut shell, &mut endpoints).unwrap();
    assert_eq!(committed.completion, ActivationCompletion::Activated);
    assert_eq!((committed.endpoint_id, committed.generation), (target, 7));
    assert_eq!(committed.effects, vec![mouse_capture(true)]);
}

fn runtime_fixture(
    serial: u64,
) -> (
    crate::client::ClientState,
    EndpointRegistry,
    SentMessages,
    SentMessages,
    Option<PendingEndpointActivation>,
) {
    use crate::client::{
        endpoint_commands::EndpointCommands, shell_runtime::begin_endpoint_activation, ClientState,
    };
    let (shell, mut endpoints, local_sent, remote_sent) = shell_and_registry();
    let mut state = ClientState::test_new();
    state.shell = Some(shell);
    let mut pending = None;
    let mut next_serial = serial;
    begin_endpoint_activation(
        &mut state,
        &mut endpoints,
        &mut EndpointCommands::default(),
        &mut pending,
        &mut next_serial,
        endpoint(),
        None,
        false,
        Instant::now(),
        &mut None,
    )
    .unwrap();
    assert!(pending.is_some());
    (state, endpoints, local_sent, remote_sent, pending)
}

fn type_during_switch(
    state: &mut crate::client::ClientState,
    endpoints: &mut EndpointRegistry,
    pending: &mut Option<PendingEndpointActivation>,
) {
    let outcome = crate::client::shell::ClientShellInput {
        requests: vec![
            crate::protocol::ClientMessage::ClientShellPaneInput {
                pane_id: "local-pane".into(),
                events: vec![text("git "), scroll()],
            },
            crate::protocol::ClientMessage::ClientShellPaneInput {
                pane_id: "local-pane".into(),
                events: vec![crate::protocol::ClientPaneInputEvent::Paste(
                    "status".into(),
                )],
            },
        ],
        ..Default::default()
    };
    crate::client::shell_runtime::finish_client_shell_input(
        state,
        outcome,
        None,
        endpoints,
        pending,
        &mut crate::client::endpoint_commands::EndpointCommands::default(),
        &mut crate::platform::RealPrefixInputSource::default(),
        &mut None,
    )
    .unwrap();
}

fn pane_input(sent: &SentMessages) -> Vec<crate::protocol::ClientMessage> {
    sent.lock()
        .unwrap()
        .iter()
        .filter(|message| {
            matches!(
                message,
                crate::protocol::ClientMessage::ClientShellPaneInput { .. }
                    | crate::protocol::ClientMessage::ClientShellPopupInput { .. }
            )
        })
        .cloned()
        .collect()
}

/// Feed the endpoint's snapshot and surface for `revision`, as its reply batch would.
fn deliver_frame(
    state: &mut crate::client::ClientState,
    pending: &mut Option<PendingEndpointActivation>,
    endpoint_id: &ClientEndpointId,
    generation: u64,
    snapshot: crate::protocol::ClientShellSnapshot,
    pane: &str,
) -> SurfaceActivationProgress {
    let revision = snapshot.revision;
    let boot_id = snapshot.boot_id.clone();
    state
        .shell
        .as_mut()
        .unwrap()
        .cache_endpoint_snapshot_inactive_for_generation(
            endpoint_id,
            generation,
            Box::new(snapshot.clone()),
        );
    let activation = pending.as_mut().unwrap();
    let geometry = activation.geometry();
    let mut frame = surface(&boot_id, revision, pane);
    frame.frame.width = geometry.cols;
    frame.frame.height = geometry.rows;
    let _ = activation.receive_snapshot(endpoint_id, generation, &snapshot);
    activation.receive_surface(endpoint_id, generation, frame)
}

#[test]
fn keys_typed_during_a_switch_reach_the_target_in_order_after_the_commit() {
    let (mut state, mut endpoints, local_sent, remote_sent, mut pending) = runtime_fixture(60);
    let target = endpoint();
    type_during_switch(&mut state, &mut endpoints, &mut pending);
    pending
        .as_mut()
        .unwrap()
        .receive_presentation_effect(&target, 7, mouse_capture(true));
    assert!(pane_input(&local_sent).is_empty());
    assert!(pane_input(&remote_sent).is_empty());
    assert!(
        state.committed_effects.is_empty(),
        "nothing of the target reaches the frozen frame"
    );

    let _ = pending.as_mut().unwrap().receive_response(
        &target,
        7,
        "client-shell-surface:60:on",
        &surface_success("client-shell-surface:60:on", true, 2),
        &mut endpoints,
    );
    let mut snapshot = test_snapshot("remote-boot", 2);
    snapshot.focused_pane_id = Some("remote-pane".into());
    assert_eq!(
        deliver_frame(
            &mut state,
            &mut pending,
            &target,
            7,
            snapshot,
            "remote-pane"
        ),
        SurfaceActivationProgress::Ready
    );
    let successor = crate::client::shell_runtime::complete_endpoint_activation(
        &mut state,
        &mut endpoints,
        &mut pending,
        &mut crate::client::endpoint_commands::EndpointCommands::default(),
    )
    .unwrap();

    assert!(successor.is_none());
    assert!(pending.is_none());
    assert!(!state.presentation_frozen);
    assert!(endpoints.active_surface_available());
    assert_eq!(
        pane_input(&remote_sent),
        vec![crate::protocol::ClientMessage::ClientShellPaneInput {
            pane_id: "remote-pane".into(),
            events: vec![
                text("git "),
                crate::protocol::ClientPaneInputEvent::Paste("status".into())
            ],
        }]
    );
    assert!(pane_input(&local_sent).is_empty());
    assert_eq!(
        remote_sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(surface_set_active)
            .collect::<Vec<_>>(),
        vec![true],
        "one activation request"
    );
    let held = state.committed_effects.iter().collect::<Vec<_>>();
    assert!(matches!(
        held.as_slice(),
        [crate::client::ClientLoopEvent::ServerMessage {
            endpoint_id,
            generation: 7,
            message,
        }] if *endpoint_id == target && **message == mouse_capture(true)
    ));
}

#[test]
fn a_rejected_target_restores_the_source_and_drops_the_switch_input() {
    let (mut state, mut endpoints, local_sent, remote_sent, mut pending) = runtime_fixture(61);
    let target = endpoint();
    type_during_switch(&mut state, &mut endpoints, &mut pending);
    pending
        .as_mut()
        .unwrap()
        .receive_presentation_effect(&target, 7, mouse_capture(true));

    let progress = pending.as_mut().unwrap().receive_response(
        &target,
        7,
        "client-shell-surface:61:on",
        &failure("client-shell-surface:61:on", "target refused"),
        &mut endpoints,
    );
    let SurfaceActivationProgress::Rejected(error) = progress else {
        panic!("expected a rejected activation, got {progress:?}");
    };
    crate::client::shell_runtime::rollback_endpoint_activation(
        &mut state,
        &mut endpoints,
        &mut pending,
        error,
    );
    let _ = pending.as_mut().unwrap().receive_response(
        &target,
        7,
        "client-shell-surface:61:rollback-target-off",
        &surface_success("client-shell-surface:61:rollback-target-off", false, 1),
        &mut endpoints,
    );
    let _ = pending.as_mut().unwrap().receive_response(
        &ClientEndpointId::Local,
        1,
        "client-shell-surface:61:rollback-source-on",
        &surface_success("client-shell-surface:61:rollback-source-on", true, 2),
        &mut endpoints,
    );
    pending.as_mut().unwrap().receive_presentation_effect(
        &ClientEndpointId::Local,
        1,
        mouse_capture(false),
    );
    assert_eq!(
        deliver_frame(
            &mut state,
            &mut pending,
            &ClientEndpointId::Local,
            1,
            test_snapshot("local-boot", 2),
            "local-pane"
        ),
        SurfaceActivationProgress::Ready
    );
    crate::client::shell_runtime::complete_endpoint_activation(
        &mut state,
        &mut endpoints,
        &mut pending,
        &mut crate::client::endpoint_commands::EndpointCommands::default(),
    )
    .unwrap();

    assert!(pending.is_none());
    assert_eq!(endpoints.active_id(), &ClientEndpointId::Local);
    assert!(endpoints.active_surface_available());
    assert!(
        pane_input(&local_sent).is_empty(),
        "input meant for the target never runs on the source"
    );
    assert!(pane_input(&remote_sent).is_empty());
    let held = state.committed_effects.iter().collect::<Vec<_>>();
    assert!(
        matches!(
            held.as_slice(),
            [crate::client::ClientLoopEvent::ServerMessage {
                endpoint_id: ClientEndpointId::Local,
                message,
                ..
            }] if **message == mouse_capture(false)
        ),
        "only the restored source's effects are applied"
    );
}

fn keyboard_report_all(enabled: bool) -> crate::protocol::ServerMessage {
    crate::protocol::ServerMessage::ClientShellKeyboardReportAll { enabled }
}

fn is_control(message: &crate::protocol::ClientMessage, expected: &str) -> bool {
    matches!(message, crate::protocol::ClientMessage::EndpointControl { kind, .. } if kind == expected)
}

/// The remote presented, then left for Local. Returns the fixture with Local committed and the
/// remote streaming in the background.
fn local_with_background_remote() -> TestFixture {
    let (mut shell, mut endpoints, local_sent, remote_sent) = shell_and_registry();
    let remote = endpoint();
    endpoints.set_surface_active(&ClientEndpointId::Local, false);
    endpoints.set_surface_active(&remote, true);
    assert!(endpoints.set_active(&remote));
    assert!(shell.activate_endpoint_projection(&remote));
    shell.set_pane_surface(surface("remote-boot", 1, "remote-pane"));

    let mut activation = PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        ClientEndpointId::Local,
        None,
        resize(),
        3,
        Instant::now(),
    )
    .unwrap();
    assert_eq!(
        activation.receive_response(
            &ClientEndpointId::Local,
            1,
            "client-shell-surface:3:on",
            &surface_success("client-shell-surface:3:on", true, 2),
            &mut endpoints,
        ),
        SurfaceActivationProgress::Pending
    );
    let local_snapshot = test_snapshot("local-boot", 2);
    activation.receive_snapshot(&ClientEndpointId::Local, 1, &local_snapshot);
    shell.set_endpoint_snapshot(&ClientEndpointId::Local, Box::new(local_snapshot));
    assert_eq!(
        activation.receive_surface(
            &ClientEndpointId::Local,
            1,
            surface("local-boot", 2, "local-pane")
        ),
        SurfaceActivationProgress::Ready
    );
    activation.complete(&mut shell, &mut endpoints).unwrap();
    endpoints.unfreeze_input();
    assert_eq!(endpoints.active_id(), &ClientEndpointId::Local);
    local_sent.lock().unwrap().clear();
    (shell, endpoints, local_sent, remote_sent)
}

#[test]
fn leaving_a_remote_streams_it_in_the_background() {
    let (_shell, endpoints, _local_sent, remote_sent) = local_with_background_remote();
    let remote = endpoint();
    let sent = remote_sent.lock().unwrap().clone();
    assert!(
        matches!(
            sent.as_slice(),
            [
                crate::protocol::ClientMessage::ClientShellFocus { focused: false },
                background,
            ] if is_control(background, crate::protocol::endpoint::SURFACE_BACKGROUND_KIND)
        ),
        "the remote keeps its stream instead of surface.set(false): {sent:?}"
    );
    assert!(!endpoints.connection(&remote).unwrap().surface_active);
    assert!(endpoints.background_surface(&remote).is_some());
}

#[test]
fn returning_to_a_background_remote_commits_before_any_reply() {
    let (mut shell, mut endpoints, local_sent, remote_sent) = local_with_background_remote();
    let remote = endpoint();
    let mut newer = surface("remote-boot", 1, "remote-pane");
    newer.surface_revision = 5;
    for message in [
        mouse_capture(true),
        keyboard_report_all(false),
        crate::protocol::ServerMessage::PaneSurface(newer.clone()),
        crate::protocol::ServerMessage::WindowTitle {
            title: Some("hidden".into()),
        },
    ] {
        assert!(
            endpoints
                .follow_background_surface(&remote, 7, Box::new(message))
                .is_none(),
            "a background remote's presentation stays with its kept surface"
        );
    }
    remote_sent.lock().unwrap().clear();

    let committed =
        present_background_surface(&mut shell, &mut endpoints, &remote, None, &resize(), 9)
            .expect("a current background surface commits at once");

    assert_eq!(committed.endpoint_id, remote);
    assert_eq!(committed.completion, ActivationCompletion::Activated);
    assert_eq!(
        committed.effects,
        vec![mouse_capture(true), keyboard_report_all(false)]
    );
    assert_eq!(endpoints.active_id(), &remote);
    assert!(endpoints.connection(&remote).unwrap().surface_active);
    assert!(endpoints.background_surface(&remote).is_none());
    assert!(shell.endpoint_is_active(&remote));
    assert_eq!(shell.followed_pane_surface(), Some(&newer));
    let sent = remote_sent.lock().unwrap().clone();
    assert!(
        matches!(
            sent.as_slice(),
            [
                foreground,
                crate::protocol::ClientMessage::ClientShellFocus { focused: true },
            ] if is_control(foreground, crate::protocol::endpoint::SURFACE_FOREGROUND_KIND)
        ),
        "no resize or surface.set(true), so the endpoint keeps its projection epoch: {sent:?}"
    );
    let released = local_sent.lock().unwrap().clone();
    assert!(released.contains(&crate::protocol::ClientMessage::ClientShellFocus { focused: false }));
    assert!(released
        .iter()
        .any(|message| surface_set_active(message) == Some(false)));
}

#[test]
fn a_stale_background_surface_falls_back_to_a_full_activation() {
    let other_geometry = crate::protocol::ClientMessage::ClientShellResize {
        cell_width_px: 8,
        cell_height_px: 16,
        surface_size: crate::protocol::ClientSurfaceSize {
            cols: 100,
            rows: 30,
        },
        pixel_mouse: false,
    };
    type Stale = fn(&mut crate::client::ClientShellState, &mut EndpointRegistry);
    let cases: [(
        &str,
        Stale,
        crate::protocol::ClientMessage,
        Option<crate::client::shell::ClientEndpointFocusTarget>,
    ); 4] = [
        ("modes not yet streamed", |_, _| {}, resize(), None),
        ("geometry changed", modes_streamed, other_geometry, None),
        (
            "snapshot ahead of the kept surface",
            |shell, endpoints| {
                modes_streamed(shell, endpoints);
                shell.cache_endpoint_snapshot_inactive_for_generation(
                    &endpoint(),
                    7,
                    Box::new(test_snapshot("remote-boot", 2)),
                );
            },
            resize(),
            None,
        ),
        (
            "navigation to a workspace the surface does not show",
            modes_streamed,
            resize(),
            Some(crate::client::shell::ClientEndpointFocusTarget::Workspace(
                "elsewhere".into(),
            )),
        ),
    ];
    for (case, make_stale, geometry, focus) in cases {
        let (mut shell, mut endpoints, local_sent, remote_sent) = local_with_background_remote();
        let remote = endpoint();
        make_stale(&mut shell, &mut endpoints);
        remote_sent.lock().unwrap().clear();

        assert!(
            present_background_surface(
                &mut shell,
                &mut endpoints,
                &remote,
                focus.as_ref(),
                &geometry,
                9
            )
            .is_none(),
            "{case}"
        );
        assert_eq!(endpoints.active_id(), &ClientEndpointId::Local, "{case}");
        assert!(remote_sent.lock().unwrap().is_empty(), "{case}");
        assert!(local_sent.lock().unwrap().is_empty(), "{case}");

        let _activation = PendingEndpointActivation::begin(
            &shell,
            &mut endpoints,
            remote.clone(),
            focus,
            geometry,
            9,
            Instant::now(),
        )
        .unwrap();
        assert!(
            endpoints.background_surface(&remote).is_none(),
            "{case}: surface.set(true) replaces the background stream"
        );
        assert!(
            remote_sent
                .lock()
                .unwrap()
                .iter()
                .any(|message| surface_set_active(message) == Some(true)),
            "{case}"
        );
    }
}

fn modes_streamed(_shell: &mut crate::client::ClientShellState, endpoints: &mut EndpointRegistry) {
    for message in [mouse_capture(false), keyboard_report_all(false)] {
        assert!(endpoints
            .follow_background_surface(&endpoint(), 7, Box::new(message))
            .is_none());
    }
}
