//! Surface ack flow control through the headless render paths.
#![cfg(unix)]

use super::*;

/// Claims everything queued for the client, as its writer would, and decodes it.
fn drain_client(writer: &ClientWriter) -> (u64, Vec<ServerMessage>) {
    let mut bytes = 0;
    let mut messages = Vec::new();
    for buffer in writer.test_drain() {
        bytes += buffer.len() as u64;
        let mut frames = buffer.as_slice();
        while !frames.is_empty() {
            messages.push(protocol::read_message(&mut frames, MAX_GRAPHICS_FRAME_SIZE).unwrap());
        }
    }
    (bytes, messages)
}

#[tokio::test]
async fn unacked_client_stalls_renders_then_converges_to_the_latest_surface() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let writer = ClientWriter::test_flow_paused();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: false,
            surface_delta: false,
            surface_scroll: false,
            surface_tab_baselines: false,
            client_id: 9,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer: writer.clone(),
        })
    );
    server.render_and_stream();
    let (mut received, _) = drain_client(&writer);

    // The client reads everything but never acks, so output stalls once the window fills.
    let sources = HashSet::from([pane_id]);
    let mut stalled = false;
    for line in 0..10_000 {
        let output = format!("\r\nline {line} {}", "x".repeat(60));
        write_shared_test_pane(&mut server, pane_id, output.as_bytes());
        server.render_retained_pane_surface_and_stream(&sources);
        let (bytes, _) = drain_client(&writer);
        received += bytes;
        if bytes == 0 {
            stalled = true;
            break;
        }
    }
    assert!(stalled, "renders never stalled without acks");
    assert_eq!(server.clients[&9].deferred_render(), DeferredRender::Full);
    assert_eq!(writer.test_flow_counts(), Some((received, 0)));

    write_shared_test_pane(&mut server, pane_id, b"\r\nLATEST");
    server.render_retained_pane_surface_and_stream(&sources);
    server.render_and_stream();
    assert_eq!(drain_client(&writer).0, 0, "still stalled");

    assert!(writer.test_acknowledge(received));
    assert!(server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 9 }));
    server.render_and_stream();
    let (_, messages) = drain_client(&writer);
    let surface = messages
        .into_iter()
        .find_map(|message| match message {
            ServerMessage::PaneSurface(surface) => Some(surface),
            _ => None,
        })
        .expect("full recovery surface after the ack");
    assert!(frame_text(&surface.frame).contains("LATEST"));
    assert_eq!(server.clients[&9].deferred_render(), DeferredRender::None);

    shutdown_test_runtimes(&mut server);
}
