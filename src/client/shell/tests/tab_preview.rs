use super::*;

const COLS: u16 = 100;
const ROWS: u16 = 28;

/// Workspace, tab, and pane of each tab: `ws_1` holds two tabs, the others one each.
const TABS: [(&str, &str, &str); 4] = [
    ("ws_1", "tab_1", "pane_1"),
    ("ws_1", "tab_3", "pane_3"),
    ("ws_2", "tab_2", "pane_2"),
    ("ws_3", "tab_4", "pane_4"),
];

fn pane_of(tab_id: &str) -> &'static str {
    TABS.iter()
        .find(|(_, tab, _)| *tab == tab_id)
        .map(|(_, _, pane)| *pane)
        .expect("known tab")
}

fn workspace_of(tab_id: &str) -> &'static str {
    TABS.iter()
        .find(|(_, tab, _)| *tab == tab_id)
        .map(|(workspace, _, _)| *workspace)
        .expect("known tab")
}

fn tabs_snapshot(focused_tab: &str, revision: u64) -> ClientShellSnapshot {
    let mut projected = snapshot();
    projected.revision = revision;
    let focused_workspace = workspace_of(focused_tab);
    let focused_pane = pane_of(focused_tab);
    let workspace = projected.workspaces[0].clone();
    projected.workspaces = ["ws_1", "ws_2", "ws_3"]
        .into_iter()
        .enumerate()
        .map(|(index, workspace_id)| {
            let active_tab = if workspace_id == focused_workspace {
                focused_tab
            } else {
                TABS.iter()
                    .find(|(ws, _, _)| *ws == workspace_id)
                    .map(|(_, tab, _)| *tab)
                    .unwrap()
            };
            ClientShellWorkspace {
                workspace_id: workspace_id.into(),
                active_tab_id: active_tab.into(),
                number: index + 1,
                label: format!("work-{}", index + 1),
                focused: workspace_id == focused_workspace,
                ..workspace.clone()
            }
        })
        .collect();
    let tab = projected.tabs[0].clone();
    let pane = projected.panes[0].clone();
    projected.tabs = TABS
        .iter()
        .map(|(workspace_id, tab_id, _)| ClientShellTab {
            tab_id: (*tab_id).into(),
            workspace_id: (*workspace_id).into(),
            label: tab_id.trim_start_matches("tab_").into(),
            focused: *tab_id == focused_tab,
            ..tab.clone()
        })
        .collect();
    projected.panes = TABS
        .iter()
        .map(|(workspace_id, tab_id, pane_id)| ClientShellPane {
            pane_id: (*pane_id).into(),
            workspace_id: (*workspace_id).into(),
            tab_id: (*tab_id).into(),
            focused: *pane_id == focused_pane,
            ..pane.clone()
        })
        .collect();
    projected.focused_workspace_id = Some(focused_workspace.into());
    projected.focused_tab_id = Some(focused_tab.into());
    projected.focused_pane_id = Some(focused_pane.into());
    projected
}

/// A full-size pane surface whose first row names its tab.
fn tab_surface(cols: u16, rows: u16, tab_id: &str, revision: u64) -> PaneSurfaceFrame {
    let state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let size = state.surface_size(cols, rows);
    let area = Rect::new(0, 0, size.cols, size.rows);
    let mut buffer = Buffer::empty(area);
    buffer.set_string(0, 0, format!("SCREEN {tab_id}"), Style::default());
    PaneSurfaceFrame {
        boot_id: "boot-1".into(),
        projection_revision: revision,
        surface_revision: revision,
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]),
        panes: vec![PaneSurfacePane {
            pane_id: pane_of(tab_id).into(),
            content_revision: 0,
            rect: area.into(),
            inner_rect: area.into(),
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
        graphics: crate::protocol::SurfaceGraphicsScene::default(),
    }
}

fn present(state: &mut ClientShellState, tab_id: &str, revision: u64) {
    state.set_snapshot(Box::new(tabs_snapshot(tab_id, revision)));
    state.set_pane_surface(tab_surface(COLS, ROWS, tab_id, revision));
    state.compose(COLS, ROWS).expect("presented frame");
}

fn request(state: &mut ClientShellState, target: ClientEndpointFocusTarget) -> String {
    let actions = state.focus_endpoint_target(target);
    let [ClientShellAction::Endpoint { request, .. }] = &actions[..] else {
        panic!("expected one endpoint request: {actions:?}");
    };
    request.id.clone()
}

fn focus_workspace(state: &mut ClientShellState, workspace_id: &str) -> String {
    request(
        state,
        ClientEndpointFocusTarget::Workspace(workspace_id.into()),
    )
}

fn settle(state: &mut ClientShellState, request_id: &str) {
    state.handle_endpoint_result(
        "boot-1",
        request_id,
        Ok(crate::api::schema::ResponseResult::Ok {}),
    );
}

/// Walks the endpoint through `tabs` in order so each tab left behind is remembered.
fn visited(tabs: &[&str]) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    present(&mut state, tabs[0], 1);
    for (revision, tab_id) in (2..).zip(&tabs[1..]) {
        let id = request(&mut state, ClientEndpointFocusTarget::Tab((*tab_id).into()));
        present(&mut state, tab_id, revision);
        settle(&mut state, &id);
        assert!(state.previewed_tab.is_none());
    }
    state
}

/// The first word pair the composed frame shows on the first row of the pane area.
fn shown_screen(state: &mut ClientShellState) -> Option<String> {
    let frame = state.compose(COLS, ROWS)?.frame;
    let area = state.layout(COLS, ROWS).pane_surface;
    let row = frame_rows(&frame)[area.y as usize]
        .chars()
        .skip(area.x as usize)
        .take(area.width as usize)
        .collect::<String>();
    Some(row.split("  ").next().unwrap_or_default().to_owned())
}

fn highlighted_workspace(state: &mut ClientShellState) -> Vec<String> {
    let buffer = state
        .compose(COLS, ROWS)
        .unwrap()
        .to_ratatui_buffer()
        .unwrap();
    state
        .hits
        .workspaces
        .iter()
        .filter(|hit| {
            (hit.rect.x..hit.rect.right())
                .any(|x| buffer[(x, hit.rect.y)].bg == state.config.palette.active_row_bg)
        })
        .map(|hit| hit.workspace_id.clone())
        .collect()
}

fn tab_bar(state: &ClientShellState) -> Vec<String> {
    state
        .hits
        .tabs
        .iter()
        .map(|(_, tab_id)| tab_id.clone())
        .collect()
}

fn typed_pane(state: &mut ClientShellState) -> String {
    let typed = state.handle_input_bytes(b"x");
    let [ClientMessage::ClientShellPaneInput { pane_id, .. }] = &typed.requests[..] else {
        panic!("expected pane input: {:?}", typed.requests);
    };
    pane_id.clone()
}

#[test]
fn switching_to_a_remembered_tab_shows_it_before_the_reply_and_takes_keys() {
    let mut state = visited(&["tab_2", "tab_1"]);
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_1"));

    focus_workspace(&mut state, "ws_2");

    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_2"));
    assert_eq!(highlighted_workspace(&mut state), ["ws_2"]);
    assert_eq!(tab_bar(&state), ["tab_2"]);
    assert!(state.pending_workspace_highlight.is_none());
    assert_eq!(typed_pane(&mut state), "pane_2");
    assert_eq!(
        state
            .hits
            .panes
            .iter()
            .map(|hit| &hit.pane_id)
            .collect::<Vec<_>>(),
        ["pane_2"]
    );
}

#[test]
fn preview_stays_until_the_tab_surface_arrives() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let id = focus_workspace(&mut state, "ws_2");

    state.set_snapshot(Box::new(tabs_snapshot("tab_2", 3)));
    assert!(state.previewed_tab.is_some());
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_2"));
    settle(&mut state, &id);
    assert!(state.previewed_tab.is_some());

    let mut surface = tab_surface(COLS, ROWS, "tab_2", 3);
    surface.frame.cells[0].symbol = "L".into();
    state.set_pane_surface(surface);
    assert!(state.previewed_tab.is_none());
    assert_eq!(shown_screen(&mut state).as_deref(), Some("LCREEN tab_2"));
    assert_eq!(typed_pane(&mut state), "pane_2");
}

#[test]
fn failed_switch_restores_the_current_tab() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let id = focus_workspace(&mut state, "ws_2");
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_2"));

    let (repaint, _) = state.handle_endpoint_result(
        "boot-1",
        &id,
        Err(ClientShellEndpointError {
            code: Some("rejected".into()),
            message: "no".into(),
        }),
    );

    assert!(repaint);
    assert!(state.previewed_tab.is_none());
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_1"));
    assert_eq!(highlighted_workspace(&mut state), ["ws_1"]);
    assert_eq!(typed_pane(&mut state), "pane_1");
}

#[test]
fn switching_to_an_unvisited_tab_waits_for_the_endpoint() {
    let mut state = visited(&["tab_1"]);
    focus_workspace(&mut state, "ws_2");

    assert!(state.previewed_tab.is_none());
    assert!(state.pending_workspace_highlight.is_none());
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_1"));
    assert_eq!(tab_bar(&state), ["tab_1", "tab_3"]);
    assert_eq!(typed_pane(&mut state), "pane_1");
}

#[test]
fn remembered_screen_of_another_size_is_not_shown() {
    for resized_after in [false, true] {
        let mut state = visited(&["tab_2", "tab_1"]);
        if !resized_after {
            state.compose(COLS + 1, ROWS).unwrap();
            focus_workspace(&mut state, "ws_2");
            assert!(state.previewed_tab.is_none());
            continue;
        }
        focus_workspace(&mut state, "ws_2");
        assert!(state.previewed_tab.is_some());
        state.compose(COLS + 1, ROWS).unwrap();
        assert!(state.previewed_tab.is_none(), "resize drops the preview");
        assert_eq!(typed_pane(&mut state), "pane_1");
    }
}

#[test]
fn settled_switch_to_another_tab_drops_the_preview() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let id = focus_workspace(&mut state, "ws_2");
    settle(&mut state, &id);
    assert!(
        state.previewed_tab.is_some(),
        "the reply alone decides nothing"
    );

    present(&mut state, "tab_4", 3);

    assert!(state.previewed_tab.is_none());
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_4"));
    assert_eq!(typed_pane(&mut state), "pane_4");
}

#[test]
fn rapid_switches_preview_the_latest_target_and_remember_only_the_left_tab() {
    let mut state = visited(&["tab_2", "tab_4", "tab_1"]);
    let remembered_b =
        state.remembered_tab_screens[&(ClientEndpointId::Local, "tab_2".into())].clone();

    focus_workspace(&mut state, "ws_2");
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_2"));
    state.set_snapshot(Box::new(tabs_snapshot("tab_2", 4)));
    focus_workspace(&mut state, "ws_3");

    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_4"));
    assert_eq!(highlighted_workspace(&mut state), ["ws_3"]);
    assert_eq!(typed_pane(&mut state), "pane_4");
    let remembered = &state.remembered_tab_screens;
    assert_eq!(
        remembered[&(ClientEndpointId::Local, "tab_1".into())].frame,
        tab_surface(COLS, ROWS, "tab_1", 3).frame
    );
    assert_eq!(
        remembered[&(ClientEndpointId::Local, "tab_2".into())],
        remembered_b
    );
}

#[test]
fn removed_tab_forgets_its_screen() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let mut projected = tabs_snapshot("tab_1", 4);
    projected.tabs.retain(|tab| tab.tab_id != "tab_2");
    projected.panes.retain(|pane| pane.tab_id != "tab_2");
    projected
        .workspaces
        .retain(|workspace| workspace.workspace_id != "ws_2");
    state.set_snapshot(Box::new(projected));

    assert!(!state
        .remembered_tab_screens
        .contains_key(&(ClientEndpointId::Local, "tab_2".into())));
}

#[test]
fn tab_switch_within_a_workspace_previews_the_same_way() {
    let mut state = visited(&["tab_3", "tab_1"]);
    let buffer = state
        .compose(COLS, ROWS)
        .unwrap()
        .to_ratatui_buffer()
        .unwrap();
    let tab_style = |state: &ClientShellState, buffer: &Buffer, tab_id: &str| {
        let (rect, _) = state
            .hits
            .tabs
            .iter()
            .find(|(_, id)| id == tab_id)
            .expect("visible tab");
        buffer[(rect.x, rect.y)].style()
    };
    let focused_style = tab_style(&state, &buffer, "tab_1");
    assert_ne!(tab_style(&state, &buffer, "tab_3"), focused_style);

    request(&mut state, ClientEndpointFocusTarget::Tab("tab_3".into()));

    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_3"));
    let buffer = state
        .compose(COLS, ROWS)
        .unwrap()
        .to_ratatui_buffer()
        .unwrap();
    assert_eq!(tab_style(&state, &buffer, "tab_3"), focused_style);
    assert_ne!(tab_style(&state, &buffer, "tab_1"), focused_style);
    assert_eq!(typed_pane(&mut state), "pane_3");
}

fn receive_screen(state: &mut ClientShellState, tab_id: &str, surface: PaneSurfaceFrame) {
    state.receive_tab_screen(&ClientEndpointId::Local, tab_id.into(), surface);
}

#[test]
fn received_screen_previews_a_never_visited_tab() {
    let mut state = visited(&["tab_1"]);
    receive_screen(&mut state, "tab_2", tab_surface(COLS, ROWS, "tab_2", 0));

    focus_workspace(&mut state, "ws_2");

    assert!(state.previewed_tab.is_some());
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_2"));
    assert_eq!(typed_pane(&mut state), "pane_2");
}

#[test]
fn received_screen_of_another_size_or_boot_is_dropped() {
    let mut state = visited(&["tab_1"]);
    receive_screen(&mut state, "tab_2", tab_surface(COLS + 1, ROWS, "tab_2", 0));
    let mut old_boot = tab_surface(COLS, ROWS, "tab_4", 0);
    old_boot.boot_id = "boot-0".into();
    receive_screen(&mut state, "tab_4", old_boot);

    assert!(state.remembered_tab_screens.is_empty());
    focus_workspace(&mut state, "ws_2");
    assert!(state.previewed_tab.is_none());
}

#[test]
fn received_screen_does_not_replace_a_remembered_one() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let mut received = tab_surface(COLS, ROWS, "tab_2", 0);
    received.frame.cells[0].symbol = "R".into();
    receive_screen(&mut state, "tab_2", received);

    focus_workspace(&mut state, "ws_2");

    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_2"));
}

fn ms(millis: u64) -> std::time::Duration {
    std::time::Duration::from_millis(millis)
}

fn next_request(
    state: &mut ClientShellState,
    start: std::time::Instant,
    at_ms: u64,
) -> Option<String> {
    state.take_tab_screen_request(COLS, ROWS, start + ms(at_ms))
}

#[test]
fn unseen_tabs_wait_for_the_surface_size_to_settle() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let start = std::time::Instant::now();

    assert_eq!(next_request(&mut state, start, 0), None);
    assert_eq!(state.timer_delay(start + ms(250)), ms(50));
    assert_eq!(next_request(&mut state, start, 299), None);
    assert_eq!(
        next_request(&mut state, start, 300).as_deref(),
        Some("tab_3")
    );
}

#[test]
fn one_tab_screen_is_in_flight_until_it_arrives() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let start = std::time::Instant::now();
    next_request(&mut state, start, 0);

    assert_eq!(
        next_request(&mut state, start, 300).as_deref(),
        Some("tab_3")
    );
    assert_eq!(next_request(&mut state, start, 301), None);
    assert_eq!(next_request(&mut state, start, 1000), None);

    receive_screen(&mut state, "tab_3", tab_surface(COLS, ROWS, "tab_3", 0));
    assert_eq!(
        next_request(&mut state, start, 1001).as_deref(),
        Some("tab_4")
    );
    assert_eq!(next_request(&mut state, start, 1002), None);
    receive_screen(&mut state, "tab_4", tab_surface(COLS, ROWS, "tab_4", 0));
    assert_eq!(next_request(&mut state, start, 1003), None);
    assert!(state
        .remembered_tab_screens
        .contains_key(&(ClientEndpointId::Local, "tab_4".into())));
}

#[test]
fn unanswered_tab_screen_is_skipped_after_a_second() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let start = std::time::Instant::now();
    next_request(&mut state, start, 0);
    assert_eq!(
        next_request(&mut state, start, 300).as_deref(),
        Some("tab_3")
    );

    assert_eq!(next_request(&mut state, start, 1299), None);
    assert_eq!(
        next_request(&mut state, start, 1300).as_deref(),
        Some("tab_4")
    );
    receive_screen(&mut state, "tab_4", tab_surface(COLS, ROWS, "tab_4", 0));
    assert_eq!(next_request(&mut state, start, 1400), None);
    assert_eq!(next_request(&mut state, start, 5000), None);
}

#[test]
fn late_screen_of_a_skipped_tab_does_not_end_the_wait_for_the_next() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let start = std::time::Instant::now();
    next_request(&mut state, start, 0);
    next_request(&mut state, start, 300);
    assert_eq!(
        next_request(&mut state, start, 1300).as_deref(),
        Some("tab_4")
    );

    receive_screen(&mut state, "tab_3", tab_surface(COLS, ROWS, "tab_3", 0));

    assert_eq!(next_request(&mut state, start, 1400), None);
}

#[test]
fn timer_delay_reports_the_in_flight_deadline() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let start = std::time::Instant::now();
    next_request(&mut state, start, 0);
    next_request(&mut state, start, 300);

    assert_eq!(state.timer_delay(start + ms(350)), ms(100));
    assert_eq!(state.timer_delay(start + ms(1250)), ms(50));
    assert_eq!(state.timer_delay(start + ms(1300)), ms(100));
}

#[test]
fn surface_size_change_drops_the_in_flight_tab_and_asks_again() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let start = std::time::Instant::now();
    next_request(&mut state, start, 0);
    assert_eq!(
        next_request(&mut state, start, 300).as_deref(),
        Some("tab_3")
    );

    let resized = |state: &mut ClientShellState, at_ms| {
        state.take_tab_screen_request(COLS + 1, ROWS, start + ms(at_ms))
    };
    assert_eq!(resized(&mut state, 400), None);
    assert_eq!(resized(&mut state, 699), None);
    assert_eq!(resized(&mut state, 700).as_deref(), Some("tab_3"));
    assert_eq!(resized(&mut state, 701), None);
}

#[test]
fn snapshot_of_a_new_boot_resets_requests() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let start = std::time::Instant::now();
    next_request(&mut state, start, 0);
    assert_eq!(
        next_request(&mut state, start, 300).as_deref(),
        Some("tab_3")
    );

    let mut rebooted = tabs_snapshot("tab_1", 9);
    rebooted.boot_id = "boot-2".into();
    state.set_snapshot(Box::new(rebooted));

    assert_eq!(
        next_request(&mut state, start, 400).as_deref(),
        Some("tab_3")
    );
    assert_eq!(next_request(&mut state, start, 401), None);
}

#[test]
fn stale_size_reply_does_not_end_the_wait_for_the_resized_request() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let start = std::time::Instant::now();
    next_request(&mut state, start, 0);
    assert_eq!(
        next_request(&mut state, start, 300).as_deref(),
        Some("tab_3")
    );
    let resized = |state: &mut ClientShellState, at_ms| {
        state.take_tab_screen_request(COLS + 1, ROWS, start + ms(at_ms))
    };
    assert_eq!(resized(&mut state, 400), None);
    assert_eq!(resized(&mut state, 700).as_deref(), Some("tab_3"));

    receive_screen(&mut state, "tab_3", tab_surface(COLS, ROWS, "tab_3", 0));

    assert_eq!(resized(&mut state, 800), None);
    assert_eq!(resized(&mut state, 1699), None);
    receive_screen(&mut state, "tab_3", tab_surface(COLS + 1, ROWS, "tab_3", 0));
    assert_eq!(resized(&mut state, 1700).as_deref(), Some("tab_2"));
}

#[test]
fn stale_boot_reply_does_not_end_the_wait_for_the_rebooted_request() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let start = std::time::Instant::now();
    next_request(&mut state, start, 0);
    assert_eq!(
        next_request(&mut state, start, 300).as_deref(),
        Some("tab_3")
    );
    let mut rebooted = tabs_snapshot("tab_1", 9);
    rebooted.boot_id = "boot-2".into();
    state.set_snapshot(Box::new(rebooted));
    assert_eq!(
        next_request(&mut state, start, 400).as_deref(),
        Some("tab_3")
    );

    receive_screen(&mut state, "tab_3", tab_surface(COLS, ROWS, "tab_3", 0));

    assert_eq!(next_request(&mut state, start, 500), None);
    let mut current = tab_surface(COLS, ROWS, "tab_3", 0);
    current.boot_id = "boot-2".into();
    receive_screen(&mut state, "tab_3", current);
    assert_eq!(
        next_request(&mut state, start, 501).as_deref(),
        Some("tab_2")
    );
}

#[test]
fn user_input_holds_back_the_next_request_for_a_second() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let start = std::time::Instant::now();
    next_request(&mut state, start, 0);
    state.note_user_input(start + ms(500));

    assert_eq!(next_request(&mut state, start, 600), None);
    assert_eq!(next_request(&mut state, start, 1499), None);
    assert_eq!(
        next_request(&mut state, start, 1500).as_deref(),
        Some("tab_3")
    );
}

#[test]
fn user_input_keeps_the_in_flight_tab_and_delays_only_the_next_request() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let start = std::time::Instant::now();
    next_request(&mut state, start, 0);
    assert_eq!(
        next_request(&mut state, start, 300).as_deref(),
        Some("tab_3")
    );

    state.note_user_input(start + ms(400));
    assert_eq!(
        state.tab_screen_request_deadline(start + ms(450)),
        Some(start + ms(1300)),
        "the reply deadline of the in-flight tab still stands"
    );

    receive_screen(&mut state, "tab_3", tab_surface(COLS, ROWS, "tab_3", 0));
    assert_eq!(next_request(&mut state, start, 500), None);
    assert_eq!(next_request(&mut state, start, 1399), None);
    assert_eq!(
        next_request(&mut state, start, 1400).as_deref(),
        Some("tab_4")
    );
}

#[test]
fn deadline_reports_the_end_of_the_input_quiet_period() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let start = std::time::Instant::now();
    next_request(&mut state, start, 0);
    state.note_user_input(start + ms(500));

    assert_eq!(
        state.tab_screen_request_deadline(start + ms(600)),
        Some(start + ms(1500))
    );
    assert_eq!(state.tab_screen_request_deadline(start + ms(1500)), None);
}

fn recorded_methods(outcome: &ClientShellInput) -> Vec<&crate::api::schema::Method> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => Some(&request.method),
            _ => None,
        })
        .collect()
}

fn record(state: &mut ClientShellState, action: crate::input::KeybindAction) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.record_binding(crate::input::KeybindMatch::Action(action), &mut outcome);
    outcome
}

#[test]
fn close_tab_during_a_preview_names_the_shown_tab() {
    let mut state = visited(&["tab_3", "tab_1"]);
    request(&mut state, ClientEndpointFocusTarget::Tab("tab_3".into()));
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_3"));

    let closed = record(&mut state, crate::input::KeybindAction::CloseTab);

    assert!(matches!(
        recorded_methods(&closed).as_slice(),
        [crate::api::schema::Method::TabClose(target)] if target.tab_id == "tab_3"
    ));
}

#[test]
fn rename_and_new_tab_during_a_preview_target_the_shown_tab() {
    let mut state = visited(&["tab_2", "tab_1"]);
    focus_workspace(&mut state, "ws_2");

    state.open_rename_tab_overlay();
    assert!(matches!(
        &state.overlay,
        Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            target: ClientRenameTarget::Tab { tab_id, .. },
            ..
        })) if tab_id == "tab_2"
    ));
    state.overlay = None;

    state.open_new_tab_overlay();
    assert!(matches!(
        &state.overlay,
        Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            target: ClientRenameTarget::NewTab { workspace_id, .. },
            ..
        })) if workspace_id == "ws_2"
    ));
    state.overlay = None;

    state.config.prompt_new_tab_name = false;
    let created = record(&mut state, crate::input::KeybindAction::NewTab);
    assert!(matches!(
        recorded_methods(&created).as_slice(),
        [crate::api::schema::Method::TabCreate(params)]
            if params.workspace_id.as_deref() == Some("ws_2")
    ));
}

#[test]
fn relative_tab_navigation_during_a_preview_starts_from_the_shown_tab() {
    let mut state = visited(&["tab_3", "tab_1"]);
    request(&mut state, ClientEndpointFocusTarget::Tab("tab_3".into()));

    let next = record(&mut state, crate::input::KeybindAction::NextTab);

    assert!(matches!(
        recorded_methods(&next).as_slice(),
        [crate::api::schema::Method::TabFocus(target)] if target.tab_id == "tab_1"
    ));
}

/// `tab_3` holds a second pane, `pane_3b`, to the right of `pane_3`, both in the snapshot and in
/// the screen the client remembers for it.
fn with_second_pane_in_tab_3(state: &mut ClientShellState) {
    let mut projected = tabs_snapshot("tab_1", 2);
    projected.panes.push(second_pane(&projected));
    state.set_snapshot(Box::new(projected));
    let surface = state
        .remembered_tab_screens
        .get_mut(&(ClientEndpointId::Local, "tab_3".into()))
        .expect("remembered tab_3");
    let mut right = surface.panes[0].clone();
    right.pane_id = "pane_3b".into();
    right.focused = false;
    let half = surface.panes[0].rect.width / 2;
    surface.panes[0].rect.width = half;
    surface.panes[0].inner_rect.width = half;
    right.rect.x = half;
    right.rect.width = half;
    right.inner_rect.x = half;
    right.inner_rect.width = half;
    surface.panes.push(right);
}

fn second_pane(projected: &ClientShellSnapshot) -> ClientShellPane {
    ClientShellPane {
        pane_id: "pane_3b".into(),
        focused: false,
        ..projected
            .panes
            .iter()
            .find(|pane| pane.pane_id == "pane_3")
            .cloned()
            .expect("pane_3")
    }
}

fn previewing_two_pane_tab_3() -> ClientShellState {
    let mut state = visited(&["tab_3", "tab_1"]);
    with_second_pane_in_tab_3(&mut state);
    give_pane_focus_style(&mut state);
    request(&mut state, ClientEndpointFocusTarget::Tab("tab_3".into()));
    assert!(state.previewed_tab.is_some());
    state
}

#[test]
fn pane_focus_inside_a_preview_without_a_focus_style_waits_for_the_endpoint() {
    use crate::api::schema::{Method, PaneDirection, PaneFocusDirectionParams};

    let unstyled = || {
        let mut state = visited(&["tab_3", "tab_1"]);
        with_second_pane_in_tab_3(&mut state);
        request(&mut state, ClientEndpointFocusTarget::Tab("tab_3".into()));
        assert!(state.previewed_tab.is_some());
        state
    };

    let mut by_pane = unstyled();
    request(
        &mut by_pane,
        ClientEndpointFocusTarget::Pane("pane_3b".into()),
    );
    assert!(by_pane.previewed_tab.is_none());
    assert!(by_pane.predicted_pane_focus.is_none());
    assert_eq!(typed_pane(&mut by_pane), "pane_1");

    let mut by_direction = unstyled();
    let mut outcome = ClientShellInput::default();
    by_direction.push_endpoint_method(
        Method::PaneFocusDirection(PaneFocusDirectionParams {
            pane_id: None,
            direction: PaneDirection::Right,
        }),
        &mut outcome,
    );
    assert!(by_direction.previewed_tab.is_none());
    assert_eq!(typed_pane(&mut by_direction), "pane_1");

    let mut same_pane = unstyled();
    request(
        &mut same_pane,
        ClientEndpointFocusTarget::Pane("pane_3".into()),
    );
    assert!(same_pane.previewed_tab.is_some());
    assert_eq!(typed_pane(&mut same_pane), "pane_3");
}

#[test]
fn focusing_a_pane_while_previewing_keeps_the_pane_the_user_left_as_previous() {
    let last_pane = |state: &mut ClientShellState| {
        let outcome = record(state, crate::input::KeybindAction::LastPane);
        match recorded_methods(&outcome).as_slice() {
            [crate::api::schema::Method::PaneFocus(target)] => Some(target.pane_id.clone()),
            _ => None,
        }
    };

    // Straight to another pane of the remembered tab, whose screen shows `pane_3` focused.
    let mut direct = visited(&["tab_3", "tab_1"]);
    with_second_pane_in_tab_3(&mut direct);
    give_pane_focus_style(&mut direct);
    focus_pane(&mut direct, "pane_3b");
    assert_eq!(typed_pane(&mut direct), "pane_3b");
    assert_eq!(direct.previous_pane_id.as_deref(), Some("pane_1"));
    assert_eq!(last_pane(&mut direct).as_deref(), Some("pane_1"));

    // Through a preview of the tab first, then a pane of it, then another.
    let mut staged = previewing_two_pane_tab_3();
    focus_pane(&mut staged, "pane_3b");
    assert_eq!(staged.previous_pane_id.as_deref(), Some("pane_1"));
    focus_pane(&mut staged, "pane_3");
    assert_eq!(staged.previous_pane_id.as_deref(), Some("pane_1"));

    // Once the endpoint focuses the tab, its snapshot has recorded the pane the user left.
    let mut confirmed = previewing_two_pane_tab_3();
    let mut projected = tabs_snapshot("tab_3", 3);
    projected.panes.push(second_pane(&projected));
    confirmed.set_snapshot(Box::new(projected));
    assert_eq!(confirmed.previous_pane_id.as_deref(), Some("pane_1"));
    assert!(confirmed.previewed_tab.is_some(), "the surface is missing");
    focus_pane(&mut confirmed, "pane_3b");
    assert_eq!(confirmed.previous_pane_id.as_deref(), Some("pane_1"));
}

#[test]
fn focusing_a_pane_of_the_previewed_tab_keeps_the_preview_and_retargets_keys() {
    let mut state = previewing_two_pane_tab_3();

    request(
        &mut state,
        ClientEndpointFocusTarget::Pane("pane_3b".into()),
    );

    assert!(state.previewed_tab.is_some());
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_3"));
    assert_eq!(typed_pane(&mut state), "pane_3b");
    assert_eq!(
        state
            .previewed_tab
            .as_ref()
            .and_then(|preview| preview.snapshot.focused_pane_id.as_deref()),
        Some("pane_3b")
    );

    let mut confirmed = tabs_snapshot("tab_3", 3);
    confirmed.panes.push(second_pane(&confirmed));
    confirmed.focused_pane_id = Some("pane_3b".into());
    state.set_snapshot(Box::new(confirmed));
    assert!(
        state.previewed_tab.is_some(),
        "the surface is still missing"
    );
    state.set_pane_surface(tab_surface(COLS, ROWS, "tab_3", 3));

    assert!(state.previewed_tab.is_none());
    assert_eq!(typed_pane(&mut state), "pane_3b");
}

#[test]
fn directional_focus_in_the_previewed_tab_keeps_the_preview() {
    use crate::api::schema::{Method, PaneDirection, PaneFocusDirectionParams};

    let focus = |state: &mut ClientShellState, direction| {
        let mut outcome = ClientShellInput::default();
        state.push_endpoint_method(
            Method::PaneFocusDirection(PaneFocusDirectionParams {
                pane_id: None,
                direction,
            }),
            &mut outcome,
        );
    };
    let mut state = previewing_two_pane_tab_3();

    focus(&mut state, PaneDirection::Left);
    assert!(state.previewed_tab.is_some());
    assert_eq!(typed_pane(&mut state), "pane_3", "nothing lies to the left");

    focus(&mut state, PaneDirection::Right);
    assert!(state.previewed_tab.is_some());
    assert_eq!(typed_pane(&mut state), "pane_3b");
}

#[test]
fn focusing_a_pane_outside_the_previewed_tab_drops_the_preview() {
    let mut state = previewing_two_pane_tab_3();

    request(&mut state, ClientEndpointFocusTarget::Pane("pane_2".into()));

    assert!(state.previewed_tab.is_none());
    assert_eq!(typed_pane(&mut state), "pane_1");
}

/// Feeds `input` to a state that has asked for no tab screen yet, then returns whether the next
/// request is still held back once the surface size has settled.
fn input_holds_back_prefetch(input: impl FnOnce(&mut ClientShellState)) -> bool {
    let mut state = visited(&["tab_2", "tab_1"]);
    let start = std::time::Instant::now();
    next_request(&mut state, start, 0);

    input(&mut state);

    let held_back = next_request(&mut state, start, 300).is_none();
    if held_back {
        // Milliseconds round down, so one more keeps the probe past the end of the hold.
        let after = start.elapsed().as_millis() as u64;
        assert_eq!(
            next_request(&mut state, start, after + 1001).as_deref(),
            Some("tab_3"),
            "the hold lasts one second"
        );
    }
    held_back
}

#[test]
fn pixel_mouse_wheel_holds_back_prefetch_for_a_second() {
    let geometry = crate::input::mouse::HostGeometry::new(
        COLS,
        ROWS,
        u32::from(COLS) * 10,
        u32::from(ROWS) * 20,
    )
    .unwrap();

    assert!(input_holds_back_prefetch(|state| {
        state.handle_pixel_mouse(b"\x1b[<64;321;241M", geometry);
    }));
}

#[test]
fn keys_and_mouse_hold_back_prefetch() {
    assert!(input_holds_back_prefetch(|state| {
        state.handle_input_bytes(b"x");
    }));
    assert!(input_holds_back_prefetch(|state| {
        state.handle_input_bytes(b"\x1b[<64;3;4M");
    }));
}

#[test]
fn host_replies_and_focus_reports_do_not_hold_back_prefetch() {
    for reply in [
        b"\x1b]4;0;rgb:1111/2222/3333\x1b\\".as_slice(),
        b"\x1b[I".as_slice(),
        b"\x1b[O".as_slice(),
        b"\x1b[6;21;10t".as_slice(),
    ] {
        assert!(
            !input_holds_back_prefetch(|state| {
                state.handle_input_bytes(reply);
            }),
            "{reply:?}"
        );
    }
}

fn reject(state: &mut ClientShellState, request_id: &str) {
    state.handle_endpoint_result(
        "boot-1",
        request_id,
        Err(ClientShellEndpointError {
            code: Some("rejected".into()),
            message: "no".into(),
        }),
    );
}

/// The endpoint presents `tab_3` at `revision` with `focused_pane` focused.
fn present_tab_3(state: &mut ClientShellState, revision: u64, focused_pane: &str) {
    let mut projected = tabs_snapshot("tab_3", revision);
    projected.panes.push(second_pane(&projected));
    projected.focused_pane_id = Some(focused_pane.into());
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(tab_surface(COLS, ROWS, "tab_3", revision));
}

#[test]
fn tab_confirmation_before_the_pane_focus_reply_keeps_the_retargeted_pane() {
    let mut state = previewing_two_pane_tab_3();
    let pane_focus = request(
        &mut state,
        ClientEndpointFocusTarget::Pane("pane_3b".into()),
    );

    present_tab_3(&mut state, 3, "pane_3");

    assert!(state.previewed_tab.is_none(), "the tab is confirmed");
    assert_eq!(typed_pane(&mut state), "pane_3b");

    present_tab_3(&mut state, 4, "pane_3b");
    settle(&mut state, &pane_focus);

    assert!(state.predicted_pane_focus.is_none());
    assert_eq!(typed_pane(&mut state), "pane_3b");
}

#[test]
fn a_focus_move_with_no_neighbor_keeps_the_pending_retarget() {
    use crate::api::schema::{Method, PaneDirection, PaneFocusDirectionParams};

    let mut state = previewing_two_pane_tab_3();
    request(
        &mut state,
        ClientEndpointFocusTarget::Pane("pane_3b".into()),
    );
    let mut outcome = ClientShellInput::default();
    state.push_endpoint_method(
        Method::PaneFocusDirection(PaneFocusDirectionParams {
            pane_id: None,
            direction: PaneDirection::Right,
        }),
        &mut outcome,
    );

    present_tab_3(&mut state, 3, "pane_3");

    assert!(state.previewed_tab.is_none(), "the tab is confirmed");
    assert_eq!(typed_pane(&mut state), "pane_3b");
}

#[test]
fn rejected_pane_focus_after_tab_confirmation_falls_back_to_the_snapshot() {
    let mut state = previewing_two_pane_tab_3();
    let pane_focus = request(
        &mut state,
        ClientEndpointFocusTarget::Pane("pane_3b".into()),
    );
    present_tab_3(&mut state, 3, "pane_3");
    assert_eq!(typed_pane(&mut state), "pane_3b");

    reject(&mut state, &pane_focus);

    assert!(state.predicted_pane_focus.is_none());
    assert_eq!(typed_pane(&mut state), "pane_3");
}

#[test]
fn rejected_pane_focus_during_a_preview_drops_the_preview() {
    let mut state = previewing_two_pane_tab_3();
    let pane_focus = request(
        &mut state,
        ClientEndpointFocusTarget::Pane("pane_3b".into()),
    );

    reject(&mut state, &pane_focus);

    assert!(state.previewed_tab.is_none());
    assert!(state.predicted_pane_focus.is_none());
}

fn give_pane_focus_style(state: &mut ClientShellState) {
    let color = ratatui::style::Color::Rgb(200, 120, 0);
    let style = crate::protocol::ClientShellPaneFocusStyle::new(
        crate::ui::PaneFocusColors {
            accent: color,
            overlay0: color,
            overlay1: color,
            surface_dim: color,
        },
        true,
    );
    state
        .snapshot
        .as_mut()
        .expect("presented snapshot")
        .pane_focus_style = Some(style);
}

fn focus_pane(state: &mut ClientShellState, pane_id: &str) -> String {
    request(state, ClientEndpointFocusTarget::Pane(pane_id.into()))
}

#[test]
fn focusing_the_focused_pane_of_a_remembered_tab_previews_that_tab() {
    let mut state = visited(&["tab_3", "tab_1"]);

    let id = focus_pane(&mut state, "pane_3");

    assert!(state.previewed_tab.is_some());
    assert!(state.predicted_pane_focus.is_none());
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_3"));
    assert_eq!(tab_bar(&state), ["tab_1", "tab_3"]);
    assert_eq!(typed_pane(&mut state), "pane_3");

    settle(&mut state, &id);
    present(&mut state, "tab_3", 3);

    assert!(state.previewed_tab.is_none(), "the tab is confirmed");
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_3"));
    assert_eq!(typed_pane(&mut state), "pane_3");
}

#[test]
fn focusing_another_pane_of_a_remembered_tab_previews_it_with_that_pane_focused() {
    let mut state = visited(&["tab_3", "tab_1"]);
    with_second_pane_in_tab_3(&mut state);
    give_pane_focus_style(&mut state);

    let id = focus_pane(&mut state, "pane_3b");

    assert!(state.previewed_tab.is_some());
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_3"));
    assert_eq!(typed_pane(&mut state), "pane_3b");
    let preview = state.previewed_tab.as_ref().expect("preview");
    assert_eq!(preview.pane_id, "pane_3b");
    assert_eq!(preview.snapshot.focused_pane_id.as_deref(), Some("pane_3b"));
    assert_eq!(preview.snapshot.focused_tab_id.as_deref(), Some("tab_3"));

    present_tab_3(&mut state, 3, "pane_3b");
    settle(&mut state, &id);

    assert!(state.previewed_tab.is_none());
    assert!(state.predicted_pane_focus.is_none());
    assert_eq!(typed_pane(&mut state), "pane_3b");
}

#[test]
fn focusing_a_pane_in_another_workspace_previews_that_workspace() {
    let mut state = visited(&["tab_2", "tab_1"]);

    focus_pane(&mut state, "pane_2");

    let preview = state.previewed_tab.as_ref().expect("preview");
    assert_eq!(
        preview.snapshot.focused_workspace_id.as_deref(),
        Some("ws_2")
    );
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_2"));
    assert_eq!(highlighted_workspace(&mut state), ["ws_2"]);
    assert_eq!(typed_pane(&mut state), "pane_2");
}

#[test]
fn focusing_a_pane_of_a_tab_with_no_usable_screen_waits_for_the_endpoint() {
    let mut unvisited = visited(&["tab_1"]);
    focus_pane(&mut unvisited, "pane_2");
    assert!(unvisited.previewed_tab.is_none());
    assert_eq!(typed_pane(&mut unvisited), "pane_1");

    // The remembered screen is zoomed on `pane_3`, so it cannot show `pane_3b`.
    let mut zoomed = visited(&["tab_3", "tab_1"]);
    let mut projected = tabs_snapshot("tab_1", 2);
    projected.panes.push(second_pane(&projected));
    zoomed.set_snapshot(Box::new(projected));
    give_pane_focus_style(&mut zoomed);
    focus_pane(&mut zoomed, "pane_3b");
    assert!(zoomed.previewed_tab.is_none());
    assert_eq!(typed_pane(&mut zoomed), "pane_1");

    // Moving focus within the remembered screen needs the client to restyle it.
    let mut unstyled = visited(&["tab_3", "tab_1"]);
    with_second_pane_in_tab_3(&mut unstyled);
    focus_pane(&mut unstyled, "pane_3b");
    assert!(unstyled.previewed_tab.is_none());
    assert_eq!(typed_pane(&mut unstyled), "pane_1");
}

#[test]
fn rejected_pane_focus_drops_the_cross_tab_preview() {
    let mut state = visited(&["tab_3", "tab_1"]);
    with_second_pane_in_tab_3(&mut state);
    give_pane_focus_style(&mut state);
    let id = focus_pane(&mut state, "pane_3b");
    assert!(state.previewed_tab.is_some());

    reject(&mut state, &id);

    assert!(state.previewed_tab.is_none());
    assert!(state.predicted_pane_focus.is_none());
    assert_eq!(shown_screen(&mut state).as_deref(), Some("SCREEN tab_1"));
    assert_eq!(typed_pane(&mut state), "pane_1");
}

/// Leaves the endpoint `state` presents for a remote, as a switch does. Returns the surface the
/// left endpoint keeps streaming in the background.
fn leave_for_remote(state: &mut ClientShellState) -> PaneSurfaceFrame {
    let kept = state.pane_surface.clone().expect("presented surface");
    let profile = crate::client::endpoint::SavedSshEndpoint {
        id: crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "Build".into(),
        target: "dev@build.example".into(),
        session: "agents".into(),
        enabled: true,
    };
    let remote = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
    let mut projected = tabs_snapshot("tab_1", 1);
    projected.boot_id = "remote-boot".into();
    state.set_endpoint_snapshot(&remote, Box::new(projected));
    assert!(state.activate_endpoint_projection(&remote));
    let mut shown = tab_surface(COLS, ROWS, "tab_1", 1);
    shown.boot_id = "remote-boot".into();
    state.set_pane_surface(shown);
    state.compose(COLS, ROWS).expect("remote frame");
    kept
}

/// Returns to Local the way a warm switch does: asks whether `target` can be shown at once,
/// presents the kept surface, then requests the target. Returns the answer.
fn return_warm(
    state: &mut ClientShellState,
    kept: PaneSurfaceFrame,
    target: ClientEndpointFocusTarget,
) -> bool {
    let previews = state.can_preview_focus_target(&ClientEndpointId::Local, 0, &kept, &target);
    assert!(state.activate_endpoint_projection_keeping_size(&ClientEndpointId::Local));
    state.set_pane_surface(kept);
    state.focus_endpoint_target(target);
    previews
}

/// `give_pane_focus_style`, kept in the endpoint's cached snapshot too.
fn cache_pane_focus_style(state: &mut ClientShellState) {
    give_pane_focus_style(state);
    let styled = state.snapshot.clone().expect("presented snapshot");
    state.set_snapshot(styled);
}

/// Splits the presented `tab_1` so it also shows an unfocused `pane_1b`.
fn with_second_pane_in_tab_1(state: &mut ClientShellState) {
    let mut projected = state.snapshot.clone().expect("presented snapshot");
    let mut second = projected
        .panes
        .iter()
        .find(|pane| pane.pane_id == "pane_1")
        .cloned()
        .expect("pane_1");
    second.pane_id = "pane_1b".into();
    second.focused = false;
    projected.panes.push(second);
    state.set_snapshot(projected);
    let surface = state.pane_surface.as_mut().expect("presented surface");
    let mut right = surface.panes[0].clone();
    right.pane_id = "pane_1b".into();
    right.focused = false;
    let half = surface.panes[0].rect.width / 2;
    surface.panes[0].rect.width = half;
    surface.panes[0].inner_rect.width = half;
    right.rect.x = half;
    right.rect.width = half;
    right.inner_rect.x = half;
    right.inner_rect.width = half;
    surface.panes.push(right);
}

#[test]
fn a_warm_switch_shows_the_target_exactly_when_the_shell_said_it_could() {
    /// The setup, the target, and the screen and pane shown at once, if any.
    type Case = (
        &'static str,
        fn() -> ClientShellState,
        ClientEndpointFocusTarget,
        Option<(&'static str, &'static str)>,
    );
    let cases: [Case; 11] = [
        (
            "remembered tab",
            || visited(&["tab_3", "tab_1"]),
            ClientEndpointFocusTarget::Tab("tab_3".into()),
            Some(("SCREEN tab_3", "pane_3")),
        ),
        (
            "workspace whose active tab is remembered",
            || visited(&["tab_2", "tab_1"]),
            ClientEndpointFocusTarget::Workspace("ws_2".into()),
            Some(("SCREEN tab_2", "pane_2")),
        ),
        (
            "pane of a remembered tab",
            || visited(&["tab_3", "tab_1"]),
            ClientEndpointFocusTarget::Pane("pane_3".into()),
            Some(("SCREEN tab_3", "pane_3")),
        ),
        (
            "unfocused pane of a remembered tab, restyled",
            || {
                let mut state = visited(&["tab_3", "tab_1"]);
                with_second_pane_in_tab_3(&mut state);
                cache_pane_focus_style(&mut state);
                state
            },
            ClientEndpointFocusTarget::Pane("pane_3b".into()),
            Some(("SCREEN tab_3", "pane_3b")),
        ),
        (
            "pane on the kept surface, restyled",
            || {
                let mut state = visited(&["tab_1"]);
                with_second_pane_in_tab_1(&mut state);
                cache_pane_focus_style(&mut state);
                state
            },
            ClientEndpointFocusTarget::Pane("pane_1b".into()),
            Some(("SCREEN tab_1", "pane_1b")),
        ),
        (
            "unfocused pane of a remembered tab, no focus style",
            || {
                let mut state = visited(&["tab_3", "tab_1"]);
                with_second_pane_in_tab_3(&mut state);
                state
            },
            ClientEndpointFocusTarget::Pane("pane_3b".into()),
            None,
        ),
        (
            "pane on the kept surface, no focus style",
            || {
                let mut state = visited(&["tab_1"]);
                with_second_pane_in_tab_1(&mut state);
                state
            },
            ClientEndpointFocusTarget::Pane("pane_1b".into()),
            None,
        ),
        (
            "tab with no remembered screen",
            || visited(&["tab_1"]),
            ClientEndpointFocusTarget::Tab("tab_3".into()),
            None,
        ),
        (
            "remembered screen of another size",
            || {
                let mut state = visited(&["tab_3", "tab_1"]);
                state
                    .remembered_tab_screens
                    .get_mut(&(ClientEndpointId::Local, "tab_3".into()))
                    .expect("remembered tab_3")
                    .frame
                    .width -= 1;
                state
            },
            ClientEndpointFocusTarget::Tab("tab_3".into()),
            None,
        ),
        (
            "unknown pane",
            || visited(&["tab_3", "tab_1"]),
            ClientEndpointFocusTarget::Pane("gone".into()),
            None,
        ),
        (
            "endpoint without the focus method",
            || {
                let mut state = visited(&["tab_3", "tab_1"]);
                state.set_endpoint_methods(Some(vec!["pane.focus".into()]));
                state
            },
            ClientEndpointFocusTarget::Tab("tab_3".into()),
            None,
        ),
    ];
    for (case, setup, target, shown) in cases {
        let mut state = setup();
        let kept = leave_for_remote(&mut state);

        let previews = return_warm(&mut state, kept, target);

        // An unsupported request's notice covers the screen beneath it.
        state.visible_endpoint_notice = None;
        let (screen, pane) = shown.unwrap_or(("SCREEN tab_1", "pane_1"));
        assert_eq!(previews, shown.is_some(), "{case}");
        assert_eq!(shown_screen(&mut state).as_deref(), Some(screen), "{case}");
        assert_eq!(typed_pane(&mut state), pane, "{case}");
    }
}

#[test]
fn a_popup_on_the_kept_surface_keeps_a_warm_switch_from_previewing_the_target() {
    let with_popup = |state: &mut ClientShellState| {
        state
            .pane_surface
            .as_mut()
            .expect("presented surface")
            .popup = surface_with_popup().popup;
    };
    let mut remembered_tab = visited(&["tab_3", "tab_1"]);
    with_popup(&mut remembered_tab);
    let mut pane_on_surface = visited(&["tab_1"]);
    with_second_pane_in_tab_1(&mut pane_on_surface);
    cache_pane_focus_style(&mut pane_on_surface);
    with_popup(&mut pane_on_surface);
    let cases = [
        (
            remembered_tab,
            ClientEndpointFocusTarget::Tab("tab_3".into()),
        ),
        (
            pane_on_surface,
            ClientEndpointFocusTarget::Pane("pane_1b".into()),
        ),
    ];
    for (mut state, target) in cases {
        let kept = leave_for_remote(&mut state);

        let previews = return_warm(&mut state, kept, target.clone());

        assert!(!previews, "{target:?}");
        assert!(state.popup_terminal_id.is_some(), "{target:?}");
        assert!(state.previewed_tab.is_none(), "{target:?}");
        assert_eq!(
            shown_screen(&mut state).as_deref(),
            Some("SCREEN tab_1"),
            "{target:?}"
        );
    }
}

/// Gives every pane of the presented snapshot an agent, focused with its pane.
fn with_agents(state: &mut ClientShellState) {
    let mut projected = state.snapshot.clone().expect("presented snapshot");
    projected.agents = projected
        .panes
        .iter()
        .map(|pane| crate::protocol::ClientShellAgent {
            pane_id: pane.pane_id.clone(),
            workspace_id: pane.workspace_id.clone(),
            tab_id: pane.tab_id.clone(),
            name: Some(format!("agent {}", pane.pane_id)),
            display_agent: None,
            agent: Some("droid".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: crate::api::schema::AgentStatus::Idle,
            state_change_seq: 0,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: pane.focused,
        })
        .collect();
    state.set_snapshot(projected);
}

/// The panes whose agent rows the sidebar highlights as focused, by machine.
fn highlighted_agents(state: &mut ClientShellState) -> Vec<(ClientEndpointId, String)> {
    let buffer = state
        .compose(COLS, ROWS)
        .expect("composed frame")
        .to_ratatui_buffer()
        .expect("frame buffer");
    state
        .hits
        .endpoint_agents
        .iter()
        .filter(|(rect, _, _)| {
            (rect.x..rect.right())
                .all(|x| buffer[(x, rect.y)].bg == state.config.palette.active_row_bg)
        })
        .map(|(_, endpoint_id, pane_id)| (endpoint_id.clone(), pane_id.clone()))
        .collect()
}

#[test]
fn a_warm_switch_to_another_machine_highlights_the_clicked_agent_at_once() {
    type Case = (&'static str, fn() -> ClientShellState, &'static str);
    let cases: [Case; 2] = [
        (
            "agent in a remembered tab",
            || {
                let mut state = visited(&["tab_3", "tab_1"]);
                with_agents(&mut state);
                state
            },
            "pane_3",
        ),
        (
            "agent on the kept surface",
            || {
                let mut state = visited(&["tab_1"]);
                with_second_pane_in_tab_1(&mut state);
                cache_pane_focus_style(&mut state);
                with_agents(&mut state);
                state
            },
            "pane_1b",
        ),
    ];
    for (case, setup, clicked) in cases {
        let mut state = setup();
        let kept = leave_for_remote(&mut state);

        assert!(
            return_warm(
                &mut state,
                kept,
                ClientEndpointFocusTarget::Pane(clicked.into())
            ),
            "{case}"
        );

        assert_eq!(typed_pane(&mut state), clicked, "{case}");
        assert_eq!(
            highlighted_agents(&mut state),
            [(ClientEndpointId::Local, clicked.to_owned())],
            "{case}: the agent the machine focused before must not flash first"
        );
    }
}
