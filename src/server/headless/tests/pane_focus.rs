use super::*;

fn focus_pane(server: &mut HeadlessServer, client_id: u64, pane_id: &str) {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_client_shell_api_request(
        client_id,
        api::ApiRequestMessage {
            request: api::schema::Request {
                id: format!("focus-{pane_id}"),
                method: api::schema::Method::PaneFocus(api::schema::PaneTarget {
                    pane_id: pane_id.into(),
                }),
            },
            respond_to,
            response_write_complete: None,
        },
    );
    let response = response_rx.recv().expect("focus response");
    assert!(
        serde_json::from_str::<api::schema::SuccessResponse>(&response).is_ok(),
        "{response}"
    );
}

/// The client redraws focus chrome from `pane_focus_style` alone, so it must reproduce the
/// server's own frame for the new focus cell for cell.
#[tokio::test]
async fn predicted_focus_chrome_matches_the_server_frame() {
    for pane_gaps in [true, false] {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("focus-chrome");
        let first = workspace.tabs[0].root_pane;
        let second = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let third = workspace.test_split(ratatui::layout::Direction::Vertical);
        let scrollback = (0..80)
            .map(|line| format!("line {line}\r\n"))
            .collect::<String>();
        for (pane, bytes) in [
            (first, scrollback.as_bytes()),
            (second, b"SECOND".as_slice()),
            (third, b"THIRD".as_slice()),
        ] {
            workspace.insert_test_runtime(
                pane,
                crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                    80,
                    23,
                    64 * 1024,
                    bytes,
                ),
            );
        }
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        for (pane, label) in [(first, "left title"), (third, "wide 界 title")] {
            let terminal_id = server.app.state.workspaces[0].tabs[0]
                .terminal_id(pane)
                .expect("pane terminal")
                .clone();
            server
                .app
                .state
                .terminals
                .get_mut(&terminal_id)
                .expect("terminal state")
                .set_manual_label(label.into());
        }
        server.app.state.pane_gaps = pane_gaps;
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        let pane_ids =
            [first, second, third].map(|pane| server.app.public_pane_id(0, pane).unwrap());

        let (control, render) = connect_matching_test_shell(&mut server, 7);
        let style = client_shell_snapshot(&control)
            .pane_focus_style
            .expect("server sends pane focus style");
        server.render_and_stream();
        let mut presented = recv_pane_surface(&render, "baseline");
        assert!(
            presented
                .panes
                .iter()
                .any(|pane| pane.scrollbar_rect.is_some()),
            "the fixture must cover a scrollbar"
        );
        let text = frame_text(&presented.frame);
        assert!(
            text.contains("left title") && text.contains("wide 界"),
            "the fixture must cover titles: {text}"
        );

        for target in [&pane_ids[0], &pane_ids[1], &pane_ids[2], &pane_ids[0]] {
            focus_pane(&mut server, 7, target);
            server.render_and_stream();
            let confirmed = recv_pane_surface(&render, "focused surface");
            assert_ne!(
                presented.frame.cells, confirmed.frame.cells,
                "focus on {target} must change the chrome"
            );
            let focused = presented
                .panes
                .iter()
                .position(|pane| &pane.pane_id == target)
                .expect("target pane in surface");
            let mut predicted = presented.frame.clone();
            crate::ui::restyle_pane_surface_focus(&mut predicted, &presented.panes, focused, style);
            let width = usize::from(confirmed.frame.width);
            for (index, (predicted, confirmed)) in predicted
                .cells
                .iter()
                .zip(&confirmed.frame.cells)
                .enumerate()
            {
                assert_eq!(
                    predicted,
                    confirmed,
                    "pane_gaps={pane_gaps} focus={target} cell ({}, {})",
                    index % width,
                    index / width
                );
            }
            assert_eq!(predicted.cells.len(), confirmed.frame.cells.len());
            presented = confirmed;
        }
        shutdown_test_runtimes(&mut server);
    }
}
