use super::*;

use std::sync::{Arc, Mutex};

use crate::client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, EndpointNegotiation, EndpointRegistry,
    EndpointTransport, ProfileId, SavedSshEndpoint,
};

#[derive(Clone)]
struct CapturingEndpointTransport(Arc<Mutex<Vec<crate::protocol::ClientMessage>>>);

impl EndpointTransport for CapturingEndpointTransport {
    fn send(&mut self, message: &crate::protocol::ClientMessage) -> std::io::Result<()> {
        self.0.lock().unwrap().push(message.clone());
        Ok(())
    }
}

fn lifecycle_negotiation() -> EndpointNegotiation {
    EndpointNegotiation::new(
        vec!["client_shell.surface.set".into()],
        vec![
            crate::protocol::endpoint::SURFACE_INTEREST_CAPABILITY.into(),
            crate::protocol::endpoint::SURFACE_ACTIVATION_EFFECTS_CAPABILITY.into(),
            crate::protocol::endpoint::PANE_FOCUS_STYLE_CAPABILITY.into(),
            crate::protocol::endpoint::SURFACE_BACKGROUND_CAPABILITY.into(),
        ],
    )
}

fn lifecycle_resize() -> crate::protocol::ClientMessage {
    crate::protocol::ClientMessage::ClientShellResize {
        cell_width_px: 8,
        cell_height_px: 16,
        surface_size: crate::protocol::ClientSurfaceSize { cols: 80, rows: 24 },
        pixel_mouse: false,
    }
}

fn request_active_surface(server: &mut HeadlessServer, client_id: u64, request_id: &str) {
    let boot_id = server.client_shell_boot_id.clone();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id,
            request: Box::new(api::schema::Request {
                id: request_id.into(),
                method: api::schema::Method::ClientShellSurfaceSet(
                    api::schema::ClientShellSurfaceSetParams { active: true },
                ),
            }),
        })
    );
}

#[tokio::test]
async fn metadata_only_shell_is_isolated_until_surface_activation() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"");
    let pane_id = server.app.session_snapshot().focused_pane_id.unwrap();
    let workspace_id = server.app.session_snapshot().focused_workspace_id.unwrap();
    let original_size = server.effective_size;
    let (writer, control_rx, render_rx) = test_client_writer();
    let client_id = 52;

    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: false,
            surface_delta: false,
            surface_scroll: false,
            surface_tab_baselines: false,
            client_id,
            surface_cols: 101,
            surface_rows: 37,
            cell_width_px: 9,
            cell_height_px: 18,
            pixel_mouse: true,
            direct_graphics: false,
            endpoint_keybindings: true,
            mouse_capture: true,
            surface_active: false,
            writer,
        })
    );
    let _ = client_shell_snapshot(&control_rx);
    assert_eq!(server.foreground_client_id, None);
    assert_eq!(server.effective_size, original_size);

    server.render_and_stream();
    assert!(render_rx.try_recv().is_err());
    assert!(server.clients[&client_id]
        .render_state
        .last_pane_surface()
        .is_none());

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id,
            pane_id,
            events: vec![crate::protocol::ClientPaneInputEvent::Paste(
                "blocked".into()
            )],
        })
    );
    assert!(input_rx.try_recv().is_err());

    let boot_id = server.client_shell_boot_id.clone();
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request: Box::new(api::schema::Request {
                id: "inactive-mutation".into(),
                method: api::schema::Method::WorkspaceFocus(api::schema::WorkspaceTarget {
                    workspace_id,
                }),
            }),
        })
    );
    let ServerMessage::ClientShellEndpointResponseChunk { data, .. } =
        read_server_message(control_rx.recv().expect("inactive mutation response"))
    else {
        panic!("expected endpoint response");
    };
    let error = serde_json::from_slice::<api::schema::ErrorResponse>(&data).unwrap();
    assert_eq!(error.error.code, "surface_inactive");

    assert!(
        server.send_to_client_shells(ServerMessage::SemanticNotification(
            crate::protocol::SemanticNotification {
                kind: crate::protocol::SemanticNotificationKind::Custom,
                title: "metadata event".into(),
                body: None,
                sound: None,
                agent: None,
                workspace_id: None,
                tab_id: None,
                pane_id: None,
                position: None,
            },
        ))
    );
    assert!(matches!(
        read_server_message(control_rx.recv().expect("metadata notification")),
        ServerMessage::SemanticNotification(_)
    ));

    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request: Box::new(api::schema::Request {
                id: "activate-surface".into(),
                method: api::schema::Method::ClientShellSurfaceSet(
                    api::schema::ClientShellSurfaceSetParams { active: true },
                ),
            }),
        })
    );
    let ServerMessage::ClientShellEndpointResponseChunk { data, .. } =
        read_server_message(control_rx.recv().expect("surface activation response"))
    else {
        panic!("expected typed surface activation response");
    };
    let activation_ack = serde_json::from_slice::<api::schema::SuccessResponse>(&data).unwrap();
    let api::schema::ResponseResult::ClientShellSurfaceSet {
        active: true,
        projection_revision: activation_floor,
    } = activation_ack.result
    else {
        panic!("expected typed surface activation result");
    };
    assert_eq!(server.foreground_client_id, Some(client_id));
    assert_eq!(server.effective_size, (101, 37));

    server.render_and_stream();
    let ServerMessage::PaneSurface(surface) =
        read_server_message(render_rx.recv().expect("activated surface"))
    else {
        panic!("expected pane surface");
    };
    assert_eq!((surface.frame.width, surface.frame.height), (101, 37));
    assert!(surface.projection_revision >= activation_floor);
    assert_eq!(surface.surface_revision, 1);
    server
        .clients
        .get_mut(&client_id)
        .expect("surface client")
        .shell_endpoint_command_in_flight = true;

    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request: Box::new(api::schema::Request {
                id: "deactivate-surface".into(),
                method: api::schema::Method::ClientShellSurfaceSet(
                    api::schema::ClientShellSurfaceSetParams { active: false },
                ),
            }),
        })
    );
    let _ = control_rx.recv().expect("surface deactivation response");
    assert!(server.clients.contains_key(&client_id));
    let (_, runtime_pane_id) = server.app.parse_pane_id(&surface.panes[0].pane_id).unwrap();
    server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, runtime_pane_id)
        .unwrap()
        .test_process_pty_bytes(b"REACTIVATED");
    assert!(
        server.render_retained_pane_surface_and_stream(&std::collections::HashSet::from([
            runtime_pane_id
        ]))
    );
    assert!(render_rx.try_recv().is_err());

    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id,
            request: Box::new(api::schema::Request {
                id: "reactivate-surface".into(),
                method: api::schema::Method::ClientShellSurfaceSet(
                    api::schema::ClientShellSurfaceSetParams { active: true },
                ),
            }),
        })
    );
    let data = loop {
        let message =
            read_server_message(control_rx.recv().expect("surface reactivation response"));
        match message {
            ServerMessage::ClientShellEndpointResponseChunk {
                request_id, data, ..
            } if request_id == "reactivate-surface" => break data,
            ServerMessage::EndpointControl { .. }
            | ServerMessage::MouseCapture { .. }
            | ServerMessage::ClientShellKeyboardReportAll { .. }
            | ServerMessage::WindowTitle { .. }
            | ServerMessage::ClientShellEndpointResponseChunk { .. } => continue,
            other => panic!("unexpected surface reactivation message: {other:?}"),
        }
    };
    let reactivation_ack = serde_json::from_slice::<api::schema::SuccessResponse>(&data).unwrap();
    let api::schema::ResponseResult::ClientShellSurfaceSet {
        active: true,
        projection_revision: reactivation_floor,
    } = reactivation_ack.result
    else {
        panic!("expected typed surface reactivation result");
    };
    assert!(reactivation_floor > activation_floor);
    server.render_and_stream();
    let ServerMessage::PaneSurface(surface) =
        read_server_message(render_rx.recv().expect("reactivated surface"))
    else {
        panic!("expected replacement pane surface");
    };
    assert!(frame_text(&surface.frame).contains("REACTIVATED"));
    assert!(surface.projection_revision >= reactivation_floor);
    assert_eq!(surface.surface_revision, 2);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn background_surface_activation_preserves_focused_viewer_geometry() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (focused_control, _) = connect_test_shell(&mut server, 7, 68, 17);
    let _ = focused_control.recv().expect("focused client snapshot");
    assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
        client_id: 7,
        focused: true,
    }));
    let focused_size = server.app.state.workspaces[0].test_runtimes[&pane_id].current_size();
    assert_eq!(focused_size, (17, 67));
    let shared_tab_id = server.shell_tab_id_for_client(7).expect("focused tab");
    assert_eq!(
        server.tab_geometry_controllers.get(&shared_tab_id),
        Some(&7)
    );

    let (writer, background_control, _) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: false,
            surface_delta: false,
            surface_scroll: false,
            surface_tab_baselines: false,
            client_id: 8,
            surface_cols: 100,
            surface_rows: 35,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: false,
            writer,
        })
    );
    let _ = background_control
        .recv()
        .expect("background client snapshot");

    request_active_surface(&mut server, 8, "activate-background-surface");
    let _ = background_control
        .recv()
        .expect("background surface activation response");
    assert_eq!(
        server.shell_tab_id_for_client(8).as_deref(),
        Some(shared_tab_id.as_str())
    );
    assert_eq!(server.clients[&7].outer_terminal_focus, Some(true));
    assert_eq!(server.clients[&8].outer_terminal_focus, None);
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&pane_id].current_size(),
        focused_size,
        "surface activation must not transiently resize a focused viewer's tab"
    );
    assert_eq!(
        server.tab_geometry_controllers.get(&shared_tab_id),
        Some(&7)
    );

    assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
        client_id: 8,
        focused: false,
    }));
    assert_eq!(server.clients[&7].outer_terminal_focus, Some(true));
    assert_eq!(server.clients[&8].outer_terminal_focus, Some(false));
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&pane_id].current_size(),
        focused_size
    );
    assert_eq!(
        server.tab_geometry_controllers.get(&shared_tab_id),
        Some(&7)
    );

    request_active_surface(&mut server, 8, "reassert-background-surface");
    let _ = background_control
        .recv()
        .expect("background surface reassertion response");
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&pane_id].current_size(),
        focused_size
    );
    assert_eq!(
        server.tab_geometry_controllers.get(&shared_tab_id),
        Some(&7)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn focused_surface_reassertion_reclaims_tab_geometry() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (focused_control, _) = connect_test_shell(&mut server, 8, 100, 35);
    let _ = focused_control.recv().expect("focused client snapshot");
    assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
        client_id: 8,
        focused: true,
    }));
    let shared_tab_id = server.shell_tab_id_for_client(8).expect("focused tab");

    let (other_control, _) = connect_test_shell(&mut server, 7, 68, 17);
    let _ = other_control.recv().expect("other client snapshot");
    assert!(server.claim_shell_tab_geometry(7, false));
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&pane_id].current_size(),
        (17, 67)
    );

    request_active_surface(&mut server, 8, "reassert-focused-surface");
    let _ = focused_control
        .recv()
        .expect("focused surface reassertion response");

    assert_eq!(server.clients[&8].outer_terminal_focus, Some(true));
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&pane_id].current_size(),
        (35, 99)
    );
    assert_eq!(
        server.tab_geometry_controllers.get(&shared_tab_id),
        Some(&8)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn surface_activation_reply_carries_modes_and_title() {
    let mut server = test_headless_server();
    let (writer, control_rx, _render_rx) = test_client_writer();
    let client_id = 63;
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: false,
            surface_delta: false,
            surface_scroll: false,
            surface_tab_baselines: false,
            client_id,
            surface_cols: 80,
            surface_rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: true,
            mouse_capture: true,
            surface_active: false,
            writer,
        })
    );
    let _ = client_shell_snapshot(&control_rx);
    server.api_window_title = Some("target title".into());
    {
        // The modes a previous surface epoch already sent must not be deduplicated away.
        let client = server.clients.get_mut(&client_id).unwrap();
        client.host_mouse_capture_active = Some(true);
        client.host_sgr_pixels_active = Some(false);
        client.host_keyboard_report_all_active = Some(false);
    }

    request_active_surface(&mut server, client_id, "activate");

    let messages = std::iter::from_fn(|| control_rx.recv_timeout(Duration::from_millis(200)).ok())
        .map(read_server_message)
        .collect::<Vec<_>>();
    assert!(matches!(
        messages.first(),
        Some(ServerMessage::ClientShellEndpointResponseChunk { request_id, .. })
            if request_id == "activate"
    ));
    assert!(messages
        .iter()
        .any(|message| matches!(message, ServerMessage::MouseCapture { enabled: true, .. })));
    assert!(messages.iter().any(|message| matches!(
        message,
        ServerMessage::ClientShellKeyboardReportAll { enabled: false }
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        ServerMessage::WindowTitle { title: Some(title) } if title == "target title"
    )));
    shutdown_test_runtimes(&mut server);
}

fn dispatch_lifecycle_messages(
    server: &mut HeadlessServer,
    client_id: u64,
    messages: Vec<crate::protocol::ClientMessage>,
) {
    for message in messages {
        let event = match message {
            crate::protocol::ClientMessage::ClientShellResize {
                cell_width_px,
                cell_height_px,
                surface_size,
                pixel_mouse,
            } => ServerEvent::ClientShellResize {
                client_id,
                cell_width_px,
                cell_height_px,
                surface_cols: surface_size.cols,
                surface_rows: surface_size.rows,
                pixel_mouse,
            },
            crate::protocol::ClientMessage::ClientShellFocus { focused } => {
                ServerEvent::ClientShellFocus { client_id, focused }
            }
            crate::protocol::ClientMessage::ClientShellEndpointRequest { boot_id, request } => {
                ServerEvent::ClientShellEndpointRequest {
                    client_id,
                    boot_id,
                    request: Box::new(serde_json::from_str(&request).unwrap()),
                }
            }
            crate::protocol::ClientMessage::ClientShellPaneInput { pane_id, events } => {
                ServerEvent::ClientShellPaneInput {
                    client_id,
                    pane_id,
                    events,
                }
            }
            crate::protocol::ClientMessage::EndpointControl { kind, .. }
                if kind == crate::protocol::endpoint::SURFACE_BACKGROUND_KIND =>
            {
                ServerEvent::ClientShellSurfaceBackground { client_id }
            }
            crate::protocol::ClientMessage::EndpointControl { kind, .. }
                if kind == crate::protocol::endpoint::SURFACE_FOREGROUND_KIND =>
            {
                ServerEvent::ClientShellSurfaceForeground { client_id }
            }
            other => panic!("unhandled lifecycle message: {other:?}"),
        };
        server.handle_server_event(event);
    }
}

/// A real two-server/client lifecycle harness. Both source-off and target-on traverse the
/// production HeadlessServer endpoint request path; the client test only routes its emitted wire
/// messages and never authors an acknowledgement, snapshot, or surface response.
#[tokio::test]
async fn two_headless_servers_drive_atomic_endpoint_handoff() {
    let mut source_server = test_headless_server();
    let _source_input = install_focused_test_runtime(&mut source_server, b"local source");
    let (source_writer, source_control, _source_render) = test_client_writer();
    let source_client_id = 78;
    assert!(
        source_server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: false,
            surface_delta: false,
            surface_scroll: false,
            surface_tab_baselines: false,
            client_id: source_client_id,
            surface_cols: 80,
            surface_rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: true,
            mouse_capture: true,
            surface_active: true,
            writer: source_writer,
        })
    );
    let source_snapshot = client_shell_snapshot(&source_control);

    let mut target_server = test_headless_server();
    let _target_input = install_focused_test_runtime(&mut target_server, b"remote target");
    let (target_writer, target_control, target_render) = test_client_writer();
    let target_client_id = 79;
    assert!(
        target_server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: false,
            surface_delta: false,
            surface_scroll: false,
            surface_tab_baselines: false,
            client_id: target_client_id,
            surface_cols: 80,
            surface_rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: true,
            mouse_capture: true,
            surface_active: false,
            writer: target_writer,
        })
    );
    let remote_snapshot = client_shell_snapshot(&target_control);

    let profile = SavedSshEndpoint {
        id: ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "Remote".into(),
        target: "dev@example.com".into(),
        session: "main".into(),
        enabled: true,
    };
    let target_id = ClientEndpointId::Ssh(profile.id.clone());
    let mut shell = crate::client::ClientShellState::new(
        crate::client::ClientShellConfig::from_config(&crate::config::Config::default()),
    );
    shell.set_endpoint_catalog(&[profile]);
    shell.set_snapshot(source_snapshot);
    shell.set_endpoint_status(&target_id, ClientEndpointStatus::Online);
    shell.set_endpoint_snapshot(&target_id, remote_snapshot);

    let source_sent = Arc::new(Mutex::new(Vec::new()));
    let target_sent = Arc::new(Mutex::new(Vec::new()));
    let mut endpoints = EndpointRegistry::new(
        CapturingEndpointTransport(source_sent.clone()),
        1,
        lifecycle_negotiation(),
    );
    endpoints.insert(
        target_id.clone(),
        CapturingEndpointTransport(target_sent.clone()),
        7,
        lifecycle_negotiation(),
        false,
    );
    let mut activation = crate::client::endpoint::PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        target_id.clone(),
        None,
        lifecycle_resize(),
        41,
        std::time::Instant::now(),
    )
    .unwrap();

    // Route the source release through a second real HeadlessServer. The activation never
    // waits for its reply.
    let mut source_release_request_id = None;
    for message in std::mem::take(&mut *source_sent.lock().unwrap()) {
        match message {
            crate::protocol::ClientMessage::ClientShellFocus { focused } => {
                assert!(
                    source_server.handle_server_event(ServerEvent::ClientShellFocus {
                        client_id: source_client_id,
                        focused,
                    })
                );
            }
            crate::protocol::ClientMessage::ClientShellEndpointRequest { boot_id, request } => {
                let request = serde_json::from_str::<api::schema::Request>(&request).unwrap();
                if matches!(
                    request.method,
                    api::schema::Method::ClientShellSurfaceSet(
                        api::schema::ClientShellSurfaceSetParams { active: false }
                    )
                ) {
                    source_release_request_id = Some(request.id.clone());
                }
                assert!(source_server.handle_server_event(
                    ServerEvent::ClientShellEndpointRequest {
                        client_id: source_client_id,
                        boot_id,
                        request: Box::new(request),
                    }
                ));
            }
            other => panic!("unexpected source lifecycle message: {other:?}"),
        }
    }
    let source_release_request_id = source_release_request_id.expect("client source-off request");
    let source_release_data = loop {
        let message = read_server_message(source_control.recv().expect("source typed release ack"));
        match message {
            ServerMessage::ClientShellEndpointResponseChunk {
                request_id, data, ..
            } if request_id == source_release_request_id => break data,
            ServerMessage::EndpointControl { .. }
            | ServerMessage::MouseCapture { .. }
            | ServerMessage::ClientShellKeyboardReportAll { .. }
            | ServerMessage::WindowTitle { .. }
            | ServerMessage::ClientShellEndpointResponseChunk { .. } => continue,
            other => panic!("unexpected source release message: {other:?}"),
        }
    };
    assert_eq!(
        activation.receive_response(
            &ClientEndpointId::Local,
            1,
            &source_release_request_id,
            &source_release_data,
            &mut endpoints,
        ),
        crate::client::endpoint::SurfaceActivationProgress::Stale
    );

    dispatch_lifecycle_messages(
        &mut target_server,
        target_client_id,
        std::mem::take(&mut *target_sent.lock().unwrap()),
    );
    assert_eq!(
        target_server.clients[&target_client_id].outer_terminal_focus,
        Some(true)
    );
    assert_eq!(
        source_server.clients[&source_client_id].outer_terminal_focus,
        Some(false)
    );
    let ServerMessage::ClientShellEndpointResponseChunk {
        request_id, data, ..
    } = read_server_message(target_control.recv().expect("target typed activation ack"))
    else {
        panic!("expected target activation acknowledgement");
    };
    assert_eq!(
        activation.receive_response(&target_id, 7, &request_id, &data, &mut endpoints),
        crate::client::endpoint::SurfaceActivationProgress::Pending
    );

    // The modes and title ride on the activation reply. The client holds them for the commit.
    let mut held_effects = Vec::new();
    while let Ok(framed) = target_control.recv_timeout(Duration::from_millis(200)) {
        let message = read_server_message(framed);
        assert!(
            crate::client::endpoint::is_presentation_effect(&message),
            "unexpected activation reply message: {message:?}"
        );
        held_effects.push(message.clone());
        activation.receive_presentation_effect(&target_id, 7, message);
    }
    assert!(held_effects
        .iter()
        .any(|message| matches!(message, ServerMessage::MouseCapture { .. })));
    assert!(held_effects
        .iter()
        .any(|message| matches!(message, ServerMessage::ClientShellKeyboardReportAll { .. })));

    target_server.render_and_stream();
    let coherent_snapshot = client_shell_snapshot(&target_control);
    let snapshot_progress = activation.receive_snapshot(&target_id, 7, &coherent_snapshot);
    shell.set_endpoint_snapshot(&target_id, coherent_snapshot);
    assert_eq!(
        snapshot_progress,
        crate::client::endpoint::SurfaceActivationProgress::Pending
    );
    let ServerMessage::PaneSurface(coherent_surface) =
        read_server_message(target_render.recv().expect("target replacement surface"))
    else {
        panic!("expected target pane surface");
    };
    assert_eq!(
        activation.receive_surface(&target_id, 7, coherent_surface),
        crate::client::endpoint::SurfaceActivationProgress::Ready
    );

    let committed = activation.complete(&mut shell, &mut endpoints).unwrap();
    assert_eq!(
        committed.completion,
        crate::client::endpoint::ActivationCompletion::Activated
    );
    assert_eq!(committed.endpoint_id, target_id);
    assert_eq!(committed.effects, held_effects);
    assert!(
        target_sent.lock().unwrap().is_empty(),
        "the commit needs nothing more from the target"
    );
    endpoints.unfreeze_input();
    assert_eq!(endpoints.active_id(), &target_id);
    endpoints.unfreeze_input();
    assert_eq!(endpoints.active_id(), &target_id);
    assert!(endpoints.active_surface_available());
    assert!(shell.endpoint_is_active(&target_id));

    target_sent.lock().unwrap().clear();
    let _returning = crate::client::endpoint::PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        ClientEndpointId::Local,
        None,
        lifecycle_resize(),
        42,
        std::time::Instant::now(),
    )
    .unwrap();
    dispatch_lifecycle_messages(
        &mut target_server,
        target_client_id,
        std::mem::take(&mut *target_sent.lock().unwrap()),
    );
    // The remote keeps streaming to this client, and the client keeps following it.
    assert!(target_server.clients[&target_client_id].shell_surface_background);
    assert!(endpoints.background_surface(&target_id).is_some());
    dispatch_lifecycle_messages(
        &mut source_server,
        source_client_id,
        std::mem::take(&mut *source_sent.lock().unwrap()),
    );
    assert_eq!(
        source_server.clients[&source_client_id].outer_terminal_focus,
        Some(true),
        "returning to Local must restore focus without a host focus event"
    );
    assert_eq!(
        target_server.clients[&target_client_id].outer_terminal_focus,
        Some(false)
    );
    shutdown_test_runtimes(&mut source_server);
    shutdown_test_runtimes(&mut target_server);
}

#[tokio::test]
async fn surface_resync_sends_a_complete_surface_to_the_active_shell() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (writer, control_rx, render_rx) = test_client_writer();
    let client_id = 53;
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: false,
            surface_delta: true,
            surface_scroll: false,
            surface_tab_baselines: false,
            client_id,
            surface_cols: 80,
            surface_rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: true,
            mouse_capture: true,
            surface_active: true,
            writer,
        })
    );
    let _ = client_shell_snapshot(&control_rx);
    server.render_and_stream();
    let ServerMessage::PaneSurface(initial) =
        read_server_message(render_rx.recv().expect("initial surface"))
    else {
        panic!("expected pane surface");
    };
    server.render_and_stream();
    assert!(
        render_rx.try_recv().is_err(),
        "an unchanged surface is not resent"
    );

    assert!(!server.handle_server_event(ServerEvent::ClientShellSurfaceResync { client_id: 99 }));
    assert!(server.handle_server_event(ServerEvent::ClientShellSurfaceResync { client_id }));
    server.render_and_stream();
    match read_server_message(render_rx.recv().expect("resync surface")) {
        ServerMessage::PaneSurface(surface) => {
            assert!(surface.surface_revision > initial.surface_revision);
            assert_eq!(surface.frame, initial.frame);
        }
        other => panic!("expected a complete pane surface, got {other:?}"),
    }
    shutdown_test_runtimes(&mut server);
}

/// One client connection as the real transport sees it: the test writer forwards on its own
/// thread, and surface deltas are decoded against the last surface, like `client::transport`.
struct TestShellConnection {
    control: std::sync::mpsc::Receiver<Vec<u8>>,
    render: std::sync::mpsc::Receiver<Vec<u8>>,
    decoder: crate::protocol::surface_reuse::Decoder,
}

impl TestShellConnection {
    /// Every message the server has queued so far, in order per channel.
    fn drain(&mut self) -> (Vec<ServerMessage>, Vec<ServerMessage>) {
        let (mut control, mut render) = (Vec::new(), Vec::new());
        let mut quiet_since = std::time::Instant::now();
        while quiet_since.elapsed() < Duration::from_millis(100) {
            let mut received = false;
            while let Ok(framed) = self.control.try_recv() {
                control.push(read_server_message(framed));
                received = true;
            }
            while let Ok(framed) = self.render.try_recv() {
                render.push(self.decoder.decode(read_server_message(framed)).unwrap());
                received = true;
            }
            if received {
                quiet_since = std::time::Instant::now();
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        (control, render)
    }
}

fn connect_presenting_shell(
    server: &mut HeadlessServer,
    client_id: u64,
) -> (TestShellConnection, crate::protocol::PaneSurfaceFrame) {
    let (writer, control, render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: false,
            surface_delta: true,
            surface_scroll: false,
            surface_tab_baselines: false,
            client_id,
            surface_cols: 80,
            surface_rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: true,
            mouse_capture: true,
            surface_active: true,
            writer,
        })
    );
    let mut connection = TestShellConnection {
        control,
        render,
        decoder: crate::protocol::surface_reuse::Decoder::new(true, false),
    };
    assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
        client_id,
        focused: true,
    }));
    server.render_and_stream();
    let surface = connection
        .drain()
        .1
        .into_iter()
        .rev()
        .find_map(|message| match message {
            ServerMessage::PaneSurface(surface) => Some(surface),
            _ => None,
        })
        .expect("presented surface");
    (connection, surface)
}

fn write_focused_test_pane(server: &mut HeadlessServer, bytes: &[u8]) -> crate::layout::PaneId {
    let pane_id = server.app.state.workspaces[0].tabs[0].root_pane;
    server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("focused pane runtime")
        .test_process_pty_bytes(bytes);
    pane_id
}

fn paste(pane_id: &str, text: &str) -> ServerEvent {
    ServerEvent::ClientShellPaneInput {
        client_id: 7,
        pane_id: pane_id.into(),
        events: vec![crate::protocol::ClientPaneInputEvent::Paste(text.into())],
    }
}

/// Surfaces a client receives after taking the lease back. A complete surface that repeats the
/// held projection and frame is the lost-baseline repaint this design must avoid; a recompute of
/// the same frame decodes to an equal one.
fn assert_no_repaint_of(held: &crate::protocol::PaneSurfaceFrame, messages: &[ServerMessage]) {
    for message in messages {
        match message {
            ServerMessage::PaneSurfacePatch(patch) => {
                assert_eq!(patch.projection_revision, held.projection_revision);
                assert!(patch.base_surface_revision >= held.surface_revision);
            }
            ServerMessage::PaneSurface(surface)
                if surface.projection_revision == held.projection_revision =>
            {
                assert!(surface.surface_revision > held.surface_revision);
                assert_eq!(
                    surface.frame, held.frame,
                    "taking the lease back must not change the held frame"
                );
            }
            ServerMessage::PaneSurface(_) => {}
            other => panic!("unexpected render message: {other:?}"),
        }
    }
}

#[tokio::test]
async fn background_surface_streams_without_the_presentation_lease() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"BASE");
    let (mut connection, presented) = connect_presenting_shell(&mut server, 7);
    let tab_id = server.shell_tab_id_for_client(7).expect("shell tab");
    assert_eq!(server.tab_geometry_controllers.get(&tab_id), Some(&7));
    assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
        client_id: 7,
        focused: false,
    }));
    server.render_and_stream();
    connection.drain();
    let runtime_pane_id = write_focused_test_pane(&mut server, b"");
    let pane_size = server.app.state.workspaces[0].test_runtimes[&runtime_pane_id].current_size();
    let baseline = server.clients[&7]
        .render_state
        .last_pane_surface()
        .expect("baseline")
        .clone();

    assert!(server.handle_server_event(ServerEvent::ClientShellSurfaceBackground { client_id: 7 }));

    let client = &server.clients[&7];
    assert!(!client.shell_surface_active);
    assert!(client.shell_surface_background);
    assert_eq!(server.foreground_client_id, None);
    assert_eq!(server.tab_geometry_controllers.get(&tab_id), None);
    assert_eq!(
        client.render_state.last_pane_surface(),
        Some(&baseline),
        "the stream keeps its baseline"
    );
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&runtime_pane_id].current_size(),
        pane_size
    );
    let (modes, _) = connection.drain();
    assert!(
        modes
            .iter()
            .any(|message| matches!(message, ServerMessage::MouseCapture { enabled: true, .. })),
        "the client needs the modes it will present with: {modes:?}"
    );
    assert!(modes
        .iter()
        .any(|message| matches!(message, ServerMessage::ClientShellKeyboardReportAll { .. })));

    assert!(!server.handle_server_event(paste(&presented.panes[0].pane_id, "blocked")));
    assert!(input_rx.try_recv().is_err());
    assert!(!server.handle_server_event(ServerEvent::ClientShellFocus {
        client_id: 7,
        focused: true,
    }));

    write_focused_test_pane(&mut server, b"\rHIDDEN");
    assert!(
        server.render_retained_pane_surface_and_stream(&std::collections::HashSet::from([
            runtime_pane_id
        ]))
    );
    match connection.drain().1.as_slice() {
        [ServerMessage::PaneSurfacePatch(patch)] => {
            assert_eq!(patch.projection_revision, baseline.projection_revision);
            assert_eq!(patch.base_surface_revision, baseline.surface_revision);
        }
        other => panic!("expected one patch on the kept baseline, got {other:?}"),
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn foreground_resumes_the_background_baseline() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"BASE");
    let (mut connection, presented) = connect_presenting_shell(&mut server, 7);
    let tab_id = server.shell_tab_id_for_client(7).expect("shell tab");
    assert!(server.handle_server_event(ServerEvent::ClientShellSurfaceBackground { client_id: 7 }));
    write_focused_test_pane(&mut server, b"\rHIDDEN");
    server.render_and_stream();
    connection.drain();
    let projection_revision = server.clients[&7].shell_projection_revision;
    let held = server.clients[&7]
        .render_state
        .last_pane_surface()
        .expect("background baseline")
        .clone();
    assert!(frame_text(&held.frame).contains("HIDDEN"));

    assert!(server.handle_server_event(ServerEvent::ClientShellSurfaceForeground { client_id: 7 }));

    let client = &server.clients[&7];
    assert!(client.shell_surface_active);
    assert!(!client.shell_surface_background);
    assert_eq!(client.shell_projection_revision, projection_revision);
    assert_eq!(client.render_state.last_pane_surface(), Some(&held));
    assert_eq!(server.foreground_client_id, Some(7));
    assert_eq!(server.tab_geometry_controllers.get(&tab_id), Some(&7));
    server.render_and_stream();
    assert_no_repaint_of(&held, &connection.drain().1);

    write_focused_test_pane(&mut server, b"\rSHOWN");
    server.render_and_stream();
    let shown = connection.drain().1;
    assert!(
        shown.iter().any(|message| matches!(
            message,
            ServerMessage::PaneSurfacePatch(_) | ServerMessage::PaneSurface(_)
        )),
        "the stream continues after the lease comes back"
    );
    server.handle_server_event(paste(&presented.panes[0].pane_id, "typed"));
    let input = input_rx.try_recv().expect("pane input after foreground");
    assert!(String::from_utf8_lossy(&input).contains("typed"));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn surface_set_inactive_ends_a_background_stream() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"BASE");
    let (mut connection, _) = connect_presenting_shell(&mut server, 7);
    assert!(server.handle_server_event(ServerEvent::ClientShellSurfaceBackground { client_id: 7 }));
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellSurfaceBackground { client_id: 7 }),
        "only a presented surface moves to the background"
    );
    let boot_id = server.client_shell_boot_id.clone();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id: 7,
            boot_id,
            request: Box::new(api::schema::Request {
                id: "off".into(),
                method: api::schema::Method::ClientShellSurfaceSet(
                    api::schema::ClientShellSurfaceSetParams { active: false },
                ),
            }),
        })
    );
    connection.drain();
    assert!(!server.clients[&7].shell_surface_background);
    assert!(server.clients[&7]
        .render_state
        .last_pane_surface()
        .is_none());
    write_focused_test_pane(&mut server, b"\rGONE");
    server.render_and_stream();
    assert!(connection.drain().1.is_empty());
    shutdown_test_runtimes(&mut server);
}

/// Route one remote message the way the client event loop does for a background endpoint.
fn route_remote_message(
    shell: &mut crate::client::ClientShellState,
    endpoints: &mut EndpointRegistry,
    remote_id: &ClientEndpointId,
    message: ServerMessage,
) {
    let Some(message) = endpoints.follow_background_surface(remote_id, 9, Box::new(message)) else {
        return;
    };
    if let ServerMessage::EndpointControl { kind, data } = *message {
        if let Ok(crate::client::endpoint::EndpointControlMessage::Snapshot(snapshot)) =
            crate::client::endpoint::decode_endpoint_control(&kind, &data)
        {
            shell.cache_endpoint_snapshot_inactive_for_generation(remote_id, 9, snapshot);
        }
    }
}

/// A remote the client left keeps streaming. Switching back presents that stream at once, and the
/// remote then continues from the very surface the client shows.
#[tokio::test]
async fn two_headless_servers_return_to_a_background_remote_without_a_round_trip() {
    let mut local_server = test_headless_server();
    let _local_input = install_focused_test_runtime(&mut local_server, b"local");
    let (local_writer, local_control, local_render) = test_client_writer();
    assert!(
        local_server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: false,
            surface_delta: false,
            surface_scroll: false,
            surface_tab_baselines: false,
            client_id: 7,
            surface_cols: 80,
            surface_rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: true,
            mouse_capture: true,
            surface_active: false,
            writer: local_writer,
        })
    );
    let local_snapshot = client_shell_snapshot(&local_control);

    let mut remote_server = test_headless_server();
    let mut remote_input = install_focused_test_runtime(&mut remote_server, b"remote");
    let (mut remote, remote_surface) = connect_presenting_shell(&mut remote_server, 7);
    let remote_snapshot = remote_server.clients[&7]
        .shell_snapshot
        .clone()
        .expect("remote snapshot");
    assert_eq!(remote_snapshot.revision, remote_surface.projection_revision);

    let profile = SavedSshEndpoint {
        id: ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "Remote".into(),
        target: "dev@example.com".into(),
        session: "main".into(),
        enabled: true,
    };
    let remote_id = ClientEndpointId::Ssh(profile.id.clone());
    let mut shell = crate::client::ClientShellState::new(
        crate::client::ClientShellConfig::from_config(&crate::config::Config::default()),
    );
    shell.set_endpoint_catalog(&[profile]);
    shell.set_snapshot(local_snapshot);
    shell.set_endpoint_status(&remote_id, ClientEndpointStatus::Online);
    shell.set_endpoint_snapshot_for_generation(&remote_id, 9, Box::new(remote_snapshot));
    assert!(shell.activate_endpoint_projection(&remote_id));
    shell.set_pane_surface(remote_surface.clone());

    let local_sent = Arc::new(Mutex::new(Vec::new()));
    let remote_sent = Arc::new(Mutex::new(Vec::new()));
    let mut endpoints = EndpointRegistry::new(
        CapturingEndpointTransport(local_sent.clone()),
        1,
        lifecycle_negotiation(),
    );
    endpoints.set_surface_active(&ClientEndpointId::Local, false);
    endpoints.insert(
        remote_id.clone(),
        CapturingEndpointTransport(remote_sent.clone()),
        9,
        lifecycle_negotiation(),
        true,
    );
    assert!(endpoints.set_active(&remote_id));

    // Leave the remote for Local through the ordinary handoff.
    let mut to_local = crate::client::endpoint::PendingEndpointActivation::begin(
        &shell,
        &mut endpoints,
        ClientEndpointId::Local,
        None,
        lifecycle_resize(),
        41,
        std::time::Instant::now(),
    )
    .unwrap();
    dispatch_lifecycle_messages(
        &mut remote_server,
        7,
        std::mem::take(&mut *remote_sent.lock().unwrap()),
    );
    assert!(remote_server.clients[&7].shell_surface_background);
    dispatch_lifecycle_messages(
        &mut local_server,
        7,
        std::mem::take(&mut *local_sent.lock().unwrap()),
    );
    while let Ok(framed) = local_control.recv_timeout(Duration::from_millis(200)) {
        match read_server_message(framed) {
            ServerMessage::ClientShellEndpointResponseChunk {
                request_id, data, ..
            } => {
                to_local.receive_response(
                    &ClientEndpointId::Local,
                    1,
                    &request_id,
                    &data,
                    &mut endpoints,
                );
            }
            effect => to_local.receive_presentation_effect(&ClientEndpointId::Local, 1, effect),
        }
    }
    local_server.render_and_stream();
    let snapshot = client_shell_snapshot(&local_control);
    to_local.receive_snapshot(&ClientEndpointId::Local, 1, &snapshot);
    shell.cache_endpoint_snapshot_inactive_for_generation(&ClientEndpointId::Local, 1, snapshot);
    let ServerMessage::PaneSurface(local_surface) =
        read_server_message(local_render.recv().expect("local surface"))
    else {
        panic!("expected local pane surface");
    };
    assert_eq!(
        to_local.receive_surface(&ClientEndpointId::Local, 1, local_surface),
        crate::client::endpoint::SurfaceActivationProgress::Ready
    );
    to_local.complete(&mut shell, &mut endpoints).unwrap();
    endpoints.unfreeze_input();
    assert_eq!(endpoints.active_id(), &ClientEndpointId::Local);

    // The hidden remote changes; the client follows its stream.
    write_focused_test_pane(&mut remote_server, b"\rWHILE-AWAY");
    remote_server.render_and_stream();
    let (control, render) = remote.drain();
    for message in control.into_iter().chain(render) {
        route_remote_message(&mut shell, &mut endpoints, &remote_id, message);
    }
    local_sent.lock().unwrap().clear();

    // Switching back commits before the remote hears anything.
    let committed = crate::client::endpoint::present_background_surface(
        &mut shell,
        &mut endpoints,
        &remote_id,
        None,
        &lifecycle_resize(),
        42,
        [
            ServerMessage::MouseCapture {
                enabled: false,
                sgr_pixels: false,
            },
            ServerMessage::ClientShellKeyboardReportAll { enabled: false },
        ],
    )
    .expect("the followed remote surface commits at once")
    .committed;
    assert_eq!(endpoints.active_id(), &remote_id);
    let server_surface = remote_server.clients[&7]
        .render_state
        .last_pane_surface()
        .expect("remote baseline")
        .clone();
    let shown = shell
        .followed_pane_surface()
        .expect("shown surface")
        .clone();
    assert_eq!(
        shown, server_surface,
        "the client shows the remote's latest frame"
    );
    assert!(frame_text(&shown.frame).contains("WHILE-AWAY"));
    assert!(committed
        .effects
        .iter()
        .any(|effect| matches!(effect, ServerMessage::MouseCapture { enabled: true, .. })));

    // The remote takes the lease back on the same baseline.
    dispatch_lifecycle_messages(
        &mut remote_server,
        7,
        std::mem::take(&mut *remote_sent.lock().unwrap()),
    );
    dispatch_lifecycle_messages(
        &mut local_server,
        7,
        std::mem::take(&mut *local_sent.lock().unwrap()),
    );
    assert!(remote_server.clients[&7].shell_surface_active);
    assert_eq!(remote_server.foreground_client_id, Some(7));
    assert_eq!(remote_server.clients[&7].outer_terminal_focus, Some(true));
    assert!(!local_server.clients[&7].shell_surface_active);
    remote_server.render_and_stream();
    assert_no_repaint_of(&shown, &remote.drain().1);

    // Keys typed right after the switch reach the remote pane.
    endpoints.send(&crate::protocol::ClientMessage::ClientShellPaneInput {
        pane_id: shown.panes[0].pane_id.clone(),
        events: vec![crate::protocol::ClientPaneInputEvent::Paste("after".into())],
    });
    dispatch_lifecycle_messages(
        &mut remote_server,
        7,
        std::mem::take(&mut *remote_sent.lock().unwrap()),
    );
    let input = remote_input.try_recv().expect("remote pane input");
    assert!(String::from_utf8_lossy(&input).contains("after"));
    shutdown_test_runtimes(&mut local_server);
    shutdown_test_runtimes(&mut remote_server);
}
