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

#[test]
fn unseen_tabs_are_requested_once_per_settled_surface_size() {
    let mut state = visited(&["tab_2", "tab_1"]);
    let start = std::time::Instant::now();
    let at = |ms| start + std::time::Duration::from_millis(ms);

    assert!(state.take_tab_screen_requests(COLS, ROWS, at(0)).is_empty());
    assert_eq!(
        state.timer_delay(at(250)),
        std::time::Duration::from_millis(50)
    );
    assert_eq!(
        state.take_tab_screen_requests(COLS, ROWS, at(300)),
        ["tab_3", "tab_4"]
    );
    assert!(state
        .take_tab_screen_requests(COLS, ROWS, at(400))
        .is_empty());
    state.set_snapshot(Box::new(tabs_snapshot("tab_1", 9)));
    assert!(state
        .take_tab_screen_requests(COLS, ROWS, at(500))
        .is_empty());

    for (step, ms) in [1000, 1100, 1200].into_iter().enumerate() {
        let cols = COLS + 1 + step as u16;
        assert!(state
            .take_tab_screen_requests(cols, ROWS, at(ms))
            .is_empty());
    }
    assert!(state
        .take_tab_screen_requests(COLS + 3, ROWS, at(1499))
        .is_empty());
    assert_eq!(
        state.take_tab_screen_requests(COLS + 3, ROWS, at(1500)),
        ["tab_3", "tab_2", "tab_4"]
    );
    assert!(state
        .take_tab_screen_requests(COLS + 3, ROWS, at(1600))
        .is_empty());
}
