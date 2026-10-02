use super::*;

fn queue_requests(server: &mut HeadlessServer, count: usize) -> std::sync::mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::unbounded_channel();
    server.app.api_rx = receiver;
    let (respond_to, responses) = std::sync::mpsc::channel();
    for index in 0..count {
        sender
            .send(api::ApiRequestMessage {
                request: api::schema::Request {
                    id: index.to_string(),
                    method: api::schema::Method::WorkspaceList(api::schema::EmptyParams::default()),
                },
                respond_to: respond_to.clone(),
                response_write_complete: None,
            })
            .unwrap();
    }
    responses
}

#[test]
fn external_api_batches_preserve_fifo_and_leave_work_for_the_next_turn() {
    let mut server = test_headless_server();
    let count = EXTERNAL_EVENT_DRAIN_LIMIT * 2 + 1;
    let responses = queue_requests(&mut server, count);
    let mut processed = 0;
    while !server.app.api_rx.is_empty() {
        server.drain_api_requests_with_shutdown_check();
        let batch: Vec<_> = responses.try_iter().collect();
        assert_eq!(
            batch.len(),
            EXTERNAL_EVENT_DRAIN_LIMIT.min(count - processed)
        );
        for response in batch {
            let response: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["id"], processed.to_string());
            assert!(response.get("result").is_some());
            processed += 1;
        }
        assert_eq!(server.app.api_rx.len(), count - processed);
    }
    assert_eq!(processed, count);
}

#[test]
fn external_server_events_yield_with_backlog_and_preserve_fifo() {
    let mut server = test_headless_server();
    let count = EXTERNAL_EVENT_DRAIN_LIMIT * 2 + 1;
    let (sender, receiver) = mpsc::channel(count);
    server.server_event_rx = receiver;
    server.server_event_tx = sender.clone();
    for client_id in 0..count as u64 {
        sender
            .try_send(ServerEvent::ClientDisconnected { client_id })
            .unwrap();
    }
    server.drain_server_events();
    assert_eq!(
        server.server_event_rx.len(),
        count - EXTERNAL_EVENT_DRAIN_LIMIT
    );
    let ServerEvent::ClientDisconnected { client_id } = server.server_event_rx.try_recv().unwrap()
    else {
        panic!("expected the next disconnect");
    };
    assert_eq!(client_id, EXTERNAL_EVENT_DRAIN_LIMIT as u64);
    server.drain_server_events();
    assert!(server.server_event_rx.is_empty());
}

#[test]
fn external_api_batch_stops_immediately_when_shutdown_is_requested() {
    let mut server = test_headless_server();
    let responses = queue_requests(&mut server, EXTERNAL_EVENT_DRAIN_LIMIT + 1);
    server.should_quit.store(true, Ordering::Release);
    assert!(!server.drain_api_requests_with_shutdown_check());
    assert_eq!(server.app.api_rx.len(), EXTERNAL_EVENT_DRAIN_LIMIT + 1);
    assert_eq!(responses.try_iter().count(), 0);
}

#[test]
fn scheduled_work_runs_between_external_api_batches() {
    let mut server = test_headless_server();
    let responses = queue_requests(&mut server, EXTERNAL_EVENT_DRAIN_LIMIT + 1);
    let now = Instant::now();
    server.app.config_diagnostic_deadline = Some(now);
    server.app.state.config_diagnostic = Some("expired diagnostic".into());

    server.drain_api_requests_with_shutdown_check();
    assert_eq!(responses.try_iter().count(), EXTERNAL_EVENT_DRAIN_LIMIT);
    assert_eq!(server.app.api_rx.len(), 1);
    assert!(server.handle_scheduled_tasks_headless(now, false));
    assert!(server.app.state.config_diagnostic.is_none());
    assert!(server.app.config_diagnostic_deadline.is_none());
    assert_eq!(server.app.api_rx.len(), 1);
}

#[tokio::test]
async fn server_loop_drains_api_backlog_and_runs_scheduled_work() {
    let mut server = test_headless_server();
    let (sender, receiver) = mpsc::unbounded_channel();
    server.app.api_rx = receiver;
    let (respond_to, responses) = std::sync::mpsc::channel();
    let count = EXTERNAL_EVENT_DRAIN_LIMIT * 2 + 1;
    for index in 0..=count {
        // A queued stop terminates the real loop without a concurrent producer
        // or a timing assertion. Earlier requests must all receive responses.
        let method = if index == count {
            api::schema::Method::ServerStop(api::schema::EmptyParams::default())
        } else {
            api::schema::Method::WorkspaceList(api::schema::EmptyParams::default())
        };
        sender
            .send(api::ApiRequestMessage {
                request: api::schema::Request {
                    id: index.to_string(),
                    method,
                },
                respond_to: respond_to.clone(),
                response_write_complete: None,
            })
            .unwrap();
    }
    server.app.config_diagnostic_deadline = Some(Instant::now());
    server.app.state.config_diagnostic = Some("expired diagnostic".into());

    tokio::time::timeout(Duration::from_secs(5), server.run())
        .await
        .expect("queued API requests must wake the server loop")
        .expect("server loop shuts down cleanly");

    let responses: Vec<_> = responses.try_iter().collect();
    assert_eq!(responses.len(), count + 1);
    for (index, response) in responses.iter().enumerate() {
        let response: serde_json::Value = serde_json::from_str(response).unwrap();
        assert_eq!(response["id"], index.to_string());
        assert!(response.get("result").is_some());
    }
    assert!(server.app.api_rx.is_empty());
    assert!(server.app.state.config_diagnostic.is_none());
    assert!(server.app.config_diagnostic_deadline.is_none());
    assert_eq!(server.app.terminal_runtimes.len(), 0);
}

#[test]
#[ignore = "manual external API burst scheduling profile"]
fn external_api_burst_profile() {
    for pane_count in [1, 15] {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("api-profile");
        for _ in 1..pane_count {
            workspace.test_split(ratatui::layout::Direction::Horizontal);
        }
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        for count in [64, 512, 4096] {
            let mut first_samples = Vec::new();
            let mut total_samples = Vec::new();
            let mut first_count = 0;
            for sample in 0..10 {
                let responses = queue_requests(&mut server, count);
                let start = Instant::now();
                server.drain_api_requests_with_shutdown_check();
                let first = start.elapsed();
                first_count = count - server.app.api_rx.len();
                while !server.app.api_rx.is_empty() {
                    server.drain_api_requests_with_shutdown_check();
                }
                let total = start.elapsed();
                assert_eq!(responses.try_iter().count(), count);
                if sample >= 3 {
                    first_samples.push(first);
                    total_samples.push(total);
                }
            }
            first_samples.sort_unstable();
            total_samples.sort_unstable();
            println!("api-burst panes={pane_count} requests={count} first_count={first_count} first_us={:.3} total_us={:.3}",
                first_samples[3].as_secs_f64() * 1e6, total_samples[3].as_secs_f64() * 1e6);
        }
    }
}

// Readiness is a kernel event, so this can only be observed end to end through
// timing. Before the listener joined the loop's select, an idle loop noticed a
// client only at its next 250 ms housekeeping wake. The bound sits well under
// that and far above the sub-millisecond accept a woken loop takes.
#[cfg(unix)]
#[tokio::test]
async fn idle_server_loop_accepts_a_new_client_without_waiting_for_its_housekeeping_wake() {
    let mut server = test_headless_server();
    // A closed API channel would wake the loop continuously.
    let (_api_tx, api_rx) = mpsc::unbounded_channel();
    server.app.api_rx = api_rx;
    let socket_path = server.client_socket_path.clone();
    let should_quit = server.should_quit.clone();
    let quit_notify = server.server_event_tx.clone();
    let client = std::thread::spawn(move || {
        // Let the loop finish startup work and park in its idle wait.
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        let mut stream = crate::ipc::connect_local_stream(&socket_path).unwrap();
        // Version 0 is rejected by the handshake thread right after accept, so
        // the reply time is the accept latency.
        protocol::write_message(
            &mut stream,
            &protocol::ClientMessage::TerminalHello {
                version: 0,
                cols: 80,
                rows: 24,
                cell_width_px: 0,
                cell_height_px: 0,
                pixel_mouse: false,
            },
        )
        .unwrap();
        let reply: ServerMessage = protocol::read_message(&mut stream, MAX_FRAME_SIZE).unwrap();
        let elapsed = started.elapsed();
        should_quit.store(true, Ordering::Release);
        quit_notify.blocking_send(ServerEvent::QuitSignal).unwrap();
        (reply, elapsed)
    });

    tokio::time::timeout(Duration::from_secs(5), server.run())
        .await
        .expect("server loop stops after the quit signal")
        .expect("server loop shuts down cleanly");

    let (reply, elapsed) = client.join().unwrap();
    assert!(
        matches!(reply, ServerMessage::Welcome { error: Some(_), .. }),
        "unexpected reply: {reply:?}"
    );
    assert!(
        elapsed < Duration::from_millis(100),
        "idle server took {elapsed:?} to accept a client"
    );
}
