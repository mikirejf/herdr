use super::*;

fn scrolling_lines(range: std::ops::Range<usize>) -> Vec<u8> {
    range
        .map(|n| {
            format!(
                "build step {n:>3}: compiled module_{n} in {}ms\r\n",
                n * 7 % 97
            )
        })
        .collect::<String>()
        .into_bytes()
}

fn apply_rows(frame: &mut FrameData, rows: &[crate::protocol::PaneSurfacePatchRow]) {
    for row in rows {
        let start = usize::from(row.y) * usize::from(frame.width) + usize::from(row.x);
        frame.cells[start..start + row.cells.len()].clone_from_slice(&row.cells);
    }
}

#[tokio::test]
async fn surface_scroll_sends_scrolling_output_as_a_shift_and_new_rows() {
    let (mut server, _control_rx, render_rx, pane_id) =
        retained_test_server_with_control(&scrolling_lines(0..40));
    server
        .clients
        .get_mut(&1)
        .expect("scroll client")
        .render_state
        .enable_surface_scroll(true);
    server.render_and_stream();
    let mut decoder = protocol::surface_reuse::Decoder::new(false, true);
    let ServerMessage::PaneSurface(mut shell) = decoder
        .decode(read_server_message(
            render_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        ))
        .expect("initial surface")
    else {
        panic!("expected the initial pane surface");
    };

    write_shared_test_pane(&mut server, pane_id, &scrolling_lines(40..42));
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    let bytes = render_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let message = read_server_message(bytes.clone());
    assert!(matches!(
        &message,
        ServerMessage::EndpointControl { kind, .. } if kind == protocol::surface_scroll::MESSAGE_KIND
    ));

    // The decoder hands the client shell an ordinary patch that reaches the
    // server's committed surface exactly.
    let ServerMessage::PaneSurfacePatch(patch) = decoder.decode(message).expect("scroll decode")
    else {
        panic!("expected an expanded pane patch");
    };
    apply_rows(&mut shell.frame, &patch.rows);
    let committed = server.clients[&1]
        .render_state
        .last_pane_surface()
        .expect("committed surface");
    assert_eq!(shell.frame.cells, committed.frame.cells);
    assert!(frame_text(&shell.frame).contains("module_41"));

    let expanded = HeadlessServer::frame_server_message(&ServerMessage::PaneSurfacePatch(patch))
        .expect("expanded frame");
    assert!(
        bytes.len() * 4 < expanded.len(),
        "scroll frame {} should be far smaller than the rows it replaces {}",
        bytes.len(),
        expanded.len()
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn surface_scroll_is_not_sent_to_a_peer_that_did_not_negotiate_it() {
    let (mut server, _control_rx, render_rx, pane_id) =
        retained_test_server_with_control(&scrolling_lines(0..40));
    server.render_and_stream();
    let _ = render_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    write_shared_test_pane(&mut server, pane_id, &scrolling_lines(40..42));
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    let message = read_server_message(render_rx.recv_timeout(Duration::from_secs(1)).unwrap());
    assert!(matches!(message, ServerMessage::PaneSurfacePatch(_)));
    shutdown_test_runtimes(&mut server);
}

fn wheel_event(
    server: &HeadlessServer,
    pane_id: crate::layout::PaneId,
    kind: protocol::ClientMouseKind,
    lines: u16,
) -> ServerEvent {
    ServerEvent::ClientShellPaneInput {
        client_id: 1,
        pane_id: server
            .app
            .public_pane_id(0, pane_id)
            .expect("public pane id"),
        events: vec![protocol::ClientPaneInputEvent::Mouse {
            kind,
            position: protocol::ClientMousePosition::Cell { column: 2, row: 2 },
            geometry: None,
            modifiers: 0,
            lines,
        }],
    }
}

#[tokio::test]
async fn wheel_scroll_streams_a_scroll_patch_equal_to_a_full_render() {
    let (mut server, _control_rx, render_rx, pane_id) =
        retained_test_server_with_scrollback(&scrolling_lines(0..60), 100_000);
    server
        .clients
        .get_mut(&1)
        .expect("scroll client")
        .render_state
        .enable_surface_scroll(true);
    server.render_and_stream();
    let mut decoder = protocol::surface_reuse::Decoder::new(false, true);
    let ServerMessage::PaneSurface(initial) = decoder
        .decode(read_server_message(
            render_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        ))
        .expect("initial surface")
    else {
        panic!("expected the initial pane surface");
    };
    let scrollbar_x = initial.panes[0].inner_rect.x + initial.panes[0].inner_rect.width;

    let event = wheel_event(&server, pane_id, protocol::ClientMouseKind::ScrollUp, 6);
    let impact = server.dispatch_server_event(event);
    assert_eq!(impact, RenderImpact::PaneScroll(pane_id));
    let request = server.app.render_dirty.take();
    assert!(!request.generic, "a wheel scroll is not a layout change");
    assert_eq!(request.pty_sources, HashSet::from([pane_id]));

    assert!(server.render_retained_pane_surface_and_stream(&request.pty_sources));
    let bytes = render_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let message = read_server_message(bytes.clone());
    assert!(matches!(
        &message,
        ServerMessage::EndpointControl { kind, .. } if kind == protocol::surface_scroll::MESSAGE_KIND
    ));
    let ServerMessage::PaneSurfacePatch(patch) = decoder.decode(message).expect("scroll decode")
    else {
        panic!("expected an expanded pane patch");
    };
    assert_eq!(
        patch.panes[0]
            .scroll
            .expect("scroll metrics")
            .offset_from_bottom,
        6
    );
    assert!(
        patch.rows.iter().any(|row| row.x == scrollbar_x),
        "the scrollbar thumb must repaint"
    );
    let expanded = HeadlessServer::frame_server_message(&ServerMessage::PaneSurfacePatch(patch))
        .expect("expanded frame");
    assert!(
        bytes.len() * 4 < expanded.len(),
        "scroll frame {} should be far smaller than the rows it replaces {}",
        bytes.len(),
        expanded.len()
    );

    let retained = server.clients[&1]
        .render_state
        .last_pane_surface()
        .expect("committed surface")
        .clone();
    assert_ne!(retained.frame.cells, initial.frame.cells);
    server
        .clients
        .get_mut(&1)
        .unwrap()
        .render_state
        .request_repaint();
    server.render_and_stream();
    let full = recv_pane_surface(&render_rx, "full comparison surface");
    assert_eq!(retained.frame, full.frame);
    assert_eq!(retained.panes, full.panes);
    assert_eq!(retained.splits, full.splits);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn wheel_scroll_that_leaves_scrollback_unchanged_renders_nothing() {
    let (mut server, _control_rx, _render_rx, pane_id) =
        retained_test_server_with_scrollback(&scrolling_lines(0..60), 100_000);
    for kind in [
        protocol::ClientMouseKind::ScrollDown,
        protocol::ClientMouseKind::ScrollUp,
    ] {
        // The pane starts at the live bottom, so the first wheel-down is a no-op;
        // a huge wheel-up then pins the pane to the top of its history.
        let lines = if kind == protocol::ClientMouseKind::ScrollUp {
            u16::MAX
        } else {
            1
        };
        let event = wheel_event(&server, pane_id, kind, lines);
        let impact = server.dispatch_server_event(event);
        if kind == protocol::ClientMouseKind::ScrollDown {
            assert_eq!(impact, RenderImpact::None);
        } else {
            assert_eq!(impact, RenderImpact::PaneScroll(pane_id));
        }
    }
    let _ = server.app.render_dirty.take();
    let event = wheel_event(&server, pane_id, protocol::ClientMouseKind::ScrollUp, 1);
    assert_eq!(server.dispatch_server_event(event), RenderImpact::None);
    assert!(
        !server.app.render_dirty.is_pending(),
        "an ineffective wheel must not queue a render"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn wheel_scroll_from_a_background_client_stays_a_full_render() {
    let (mut server, _control_rx, _render_rx, pane_id) =
        retained_test_server_with_scrollback(&scrolling_lines(0..60), 100_000);
    server.clients.insert(
        2,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            2,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    assert!(server.promote_client_to_foreground(2));
    let _ = server.app.render_dirty.take();

    let event = wheel_event(&server, pane_id, protocol::ClientMouseKind::ScrollUp, 3);
    let impact = server.dispatch_server_event(event);
    assert_eq!(impact, RenderImpact::Full);
    assert_eq!(server.foreground_client_id, Some(1));
    assert!(
        !server.app.render_dirty.is_pending(),
        "a full render carries its own flags"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn drained_wheel_scroll_queues_the_pane_and_wakes_the_loop() {
    let (mut server, _control_rx, _render_rx, pane_id) =
        retained_test_server_with_scrollback(&scrolling_lines(0..60), 100_000);
    let event = wheel_event(&server, pane_id, protocol::ClientMouseKind::ScrollUp, 2);
    server.server_event_tx.try_send(event).unwrap();

    let impact = server.drain_server_events();
    assert_eq!(impact, RenderImpact::PaneScroll(pane_id));
    assert!(impact.needs_render());
    assert!(!impact.needs_full_render());
    assert_eq!(
        server.app.render_dirty.take().pty_sources,
        HashSet::from([pane_id])
    );
    tokio::time::timeout(Duration::from_secs(1), server.app.render_notify.notified())
        .await
        .expect("a queued scroll wakes the render loop");

    assert_eq!(
        RenderImpact::None.merge(RenderImpact::PaneScroll(pane_id)),
        RenderImpact::PaneScroll(pane_id)
    );
    assert_eq!(
        RenderImpact::PaneScroll(pane_id).merge(RenderImpact::Full),
        RenderImpact::Full
    );
    shutdown_test_runtimes(&mut server);
}
