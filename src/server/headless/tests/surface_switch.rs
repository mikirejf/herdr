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
