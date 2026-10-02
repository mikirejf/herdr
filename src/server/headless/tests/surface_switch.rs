use super::*;

use crate::protocol::surface_switch::{self, SurfaceSwitch};

fn workspace_with_screen(name: &str, screen: &[u8]) -> crate::workspace::Workspace {
    let mut workspace = crate::workspace::Workspace::test_new(name);
    let pane_id = workspace.focused_pane_id().expect("focused pane");
    workspace.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, screen),
    );
    workspace
}

async fn focus_workspace(server: &mut HeadlessServer, client_id: u64, workspace_index: usize) {
    let workspace_id = server.app.public_workspace_id(workspace_index);
    server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
        client_id,
        boot_id: server.client_shell_boot_id.clone(),
        request: Box::new(api::schema::Request {
            id: format!("focus-{workspace_index}"),
            method: api::schema::Method::WorkspaceFocus(api::schema::WorkspaceTarget {
                workspace_id,
            }),
        }),
    });
    while server.clients[&client_id].shell_endpoint_command_in_flight {
        let event = tokio::time::timeout(Duration::from_secs(1), server.server_event_rx.recv())
            .await
            .expect("focus response")
            .expect("server events");
        server.handle_server_event(event);
    }
}

struct Write {
    bytes: usize,
    switch: Option<SurfaceSwitch>,
    body: ServerMessage,
    surface: protocol::PaneSurfaceFrame,
}

/// Every render write queued so far, decoded frame by frame like the client reader.
fn drain_writes(
    render: &std::sync::mpsc::Receiver<Vec<u8>>,
    decoder: &mut protocol::surface_reuse::Decoder,
) -> Vec<Write> {
    let mut writes = Vec::new();
    while let Ok(framed) = render.recv_timeout(Duration::from_millis(100)) {
        let mut reader = framed.as_slice();
        let mut switch = None;
        while !reader.is_empty() {
            let message: ServerMessage =
                protocol::read_message(&mut reader, protocol::MAX_GRAPHICS_FRAME_SIZE).unwrap();
            match decoder.decode_frame(message.clone()).expect("decode frame") {
                None => {
                    let ServerMessage::EndpointControl { data, .. } = &message else {
                        panic!("only a switch header yields nothing");
                    };
                    switch = Some(surface_switch::decode(data).unwrap());
                }
                Some(ServerMessage::PaneSurface(surface)) => writes.push(Write {
                    bytes: framed.len(),
                    switch: switch.take(),
                    body: message,
                    surface,
                }),
                Some(other) => panic!("expected a pane surface, got {other:?}"),
            }
        }
        assert!(switch.is_none(), "a header arrives with its surface");
    }
    writes
}

#[tokio::test]
async fn returning_to_a_workspace_restores_its_parked_surface() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![
        workspace_with_screen("alpha", b"ALPHA screen"),
        workspace_with_screen("beta", b"BETA screen"),
    ];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let (writer, _control, render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: true,
            surface_delta: true,
            surface_scroll: false,
            surface_tab_baselines: true,
            client_id: 7,
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
    let mut decoder = protocol::surface_reuse::Decoder::new(true, false).with_tab_baselines(true);
    server.render_and_stream();
    let first = drain_writes(&render, &mut decoder)
        .pop()
        .expect("first surface");
    assert!(first.switch.is_none());
    assert!(matches!(first.body, ServerMessage::PaneSurface(_)));
    assert!(frame_text(&first.surface.frame).contains("ALPHA"));

    focus_workspace(&mut server, 7, 1).await;
    server.render_and_stream();
    let away = drain_writes(&render, &mut decoder)
        .pop()
        .expect("surface after leaving");
    assert!(frame_text(&away.surface.frame).contains("BETA"));
    assert_eq!(
        away.switch,
        Some(SurfaceSwitch {
            keep: vec![first.surface.surface_revision],
            restore: None,
        })
    );
    assert!(matches!(away.body, ServerMessage::PaneSurface(_)));

    server.app.state.workspaces[0]
        .test_runtimes
        .values()
        .next()
        .expect("alpha runtime")
        .test_process_pty_bytes(b"\r\nALPHA kept working");
    focus_workspace(&mut server, 7, 0).await;
    server.render_and_stream();
    let back = drain_writes(&render, &mut decoder)
        .pop()
        .expect("surface after return");
    assert_eq!(
        back.switch.as_ref().and_then(|switch| switch.restore),
        Some(first.surface.surface_revision)
    );
    assert!(
        matches!(&back.body, ServerMessage::EndpointControl { kind, .. }
        if kind == protocol::surface_delta::MESSAGE_KIND)
    );
    assert!(back.bytes * 4 < first.bytes);
    assert!(frame_text(&back.surface.frame).contains("ALPHA kept working"));
    let committed = server.clients[&7]
        .render_state
        .last_pane_surface()
        .expect("committed surface");
    assert_eq!(&back.surface, committed);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn tab_screens_leave_focus_and_surface_baselines_alone() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![
        workspace_with_screen("alpha", b"ALPHA screen"),
        workspace_with_screen("beta", b"BETA screen"),
    ];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let (writer, control, render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: true,
            surface_delta: true,
            surface_scroll: false,
            surface_tab_baselines: true,
            client_id: 7,
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
    let mut decoder = protocol::surface_reuse::Decoder::new(true, false).with_tab_baselines(true);
    server.render_and_stream();
    let first = drain_writes(&render, &mut decoder)
        .pop()
        .expect("first surface");
    let alpha_tab = server.app.public_tab_id(0, 0).expect("alpha tab");
    let beta_tab = server.app.public_tab_id(1, 0).expect("beta tab");

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellTabScreens {
            client_id: 7,
            tab_ids: vec![alpha_tab.clone(), "t_missing_1".into(), beta_tab.clone()],
        })
    );

    let mut screens = Vec::new();
    // The writer thread forwards control frames asynchronously; wait long for the first screen.
    let mut wait = Duration::from_secs(1);
    while let Ok(framed) = control.recv_timeout(wait) {
        if let ServerMessage::EndpointControl { kind, data } = read_server_message(framed) {
            if kind == protocol::tab_screens::MESSAGE_KIND {
                screens.push(protocol::tab_screens::decode(&data).expect("tab screen"));
                wait = Duration::from_millis(100);
            }
        }
    }
    let [(tab_id, screen)] = &screens[..] else {
        panic!("expected only the unfocused tab's screen: {screens:?}");
    };
    assert_eq!(tab_id, &beta_tab);
    assert_eq!(screen.boot_id, server.client_shell_boot_id);
    assert_eq!((screen.frame.width, screen.frame.height), (80, 24));
    assert!(frame_text(&screen.frame).contains("BETA screen"));
    assert!(screen.panes.iter().any(|pane| pane.focused));
    assert_eq!(server.shell_tab_id_for_client(7), Some(alpha_tab));
    assert!(render.try_recv().is_err());

    // The connection streams as if the screens were never sent.
    focus_workspace(&mut server, 7, 1).await;
    server.render_and_stream();
    let away = drain_writes(&render, &mut decoder)
        .pop()
        .expect("surface after leaving");
    assert!(frame_text(&away.surface.frame).contains("BETA screen"));
    assert_eq!(
        away.switch,
        Some(SurfaceSwitch {
            keep: vec![first.surface.surface_revision],
            restore: None,
        })
    );
    focus_workspace(&mut server, 7, 0).await;
    server.render_and_stream();
    let back = drain_writes(&render, &mut decoder)
        .pop()
        .expect("surface after return");
    assert_eq!(
        back.switch.as_ref().and_then(|switch| switch.restore),
        Some(first.surface.surface_revision)
    );
    assert!(frame_text(&back.surface.frame).contains("ALPHA screen"));
    shutdown_test_runtimes(&mut server);
}

fn connect_surface_client(
    server: &mut HeadlessServer,
    client_id: u64,
) -> (
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (writer, control, render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: true,
            surface_delta: true,
            surface_scroll: false,
            surface_tab_baselines: true,
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
    (control, render)
}

#[tokio::test]
async fn tab_screen_preview_leaves_pending_output_for_the_retained_render() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![
        workspace_with_screen("alpha", b"ALPHA screen"),
        workspace_with_screen("beta", b"BETA screen"),
    ];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let (_first_control, first_render) = connect_surface_client(&mut server, 7);
    let (_second_control, second_render) = connect_surface_client(&mut server, 8);
    focus_workspace(&mut server, 8, 1).await;
    server.render_and_stream();
    let beta_tab = server.app.public_tab_id(1, 0).expect("beta tab");
    assert_ne!(server.shell_tab_id_for_client(7), Some(beta_tab.clone()));
    assert_eq!(server.shell_tab_id_for_client(8), Some(beta_tab.clone()));
    let mut first_decoder =
        protocol::surface_reuse::Decoder::new(true, false).with_tab_baselines(true);
    let mut second_decoder =
        protocol::surface_reuse::Decoder::new(true, false).with_tab_baselines(true);
    drain_writes(&first_render, &mut first_decoder);
    drain_writes(&second_render, &mut second_decoder);

    let beta_pane = server.app.state.workspaces[1]
        .focused_pane_id()
        .expect("beta pane");
    server.app.state.workspaces[1].test_runtimes[&beta_pane]
        .test_process_pty_bytes(b"\r\nBETA fresh output");

    // Client 7 previews the tab client 8 is looking at, before the output is streamed.
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellTabScreens {
            client_id: 7,
            tab_ids: vec![beta_tab],
        })
    );
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([beta_pane])));

    let committed = server.clients[&8]
        .render_state
        .last_pane_surface()
        .expect("committed surface")
        .clone();
    let cell_size = server.clients[&8].cell_size;
    let target = server.shell_target_for_client(8);
    let fresh = render_client_shell_pane_surface(
        &mut server.app,
        target,
        Rect::new(0, 0, 80, 24),
        false,
        false,
        cell_size,
        &Default::default(),
        8,
    )
    .expect("fresh render");
    assert!(frame_text(&fresh.frame).contains("BETA fresh output"));
    assert_eq!(frame_text(&committed.frame), frame_text(&fresh.frame));
    assert!(
        committed.frame.cells == fresh.frame.cells,
        "the retained surface must equal a fresh full render"
    );
    shutdown_test_runtimes(&mut server);
}
