use super::*;
use crate::protocol::ClientShellPaneFocusStyle;
use ratatui::style::Color;

const ACCENT: Color = Color::Rgb(200, 120, 0);
const OVERLAY0: Color = Color::Rgb(70, 70, 70);

fn focus_style() -> ClientShellPaneFocusStyle {
    ClientShellPaneFocusStyle::new(
        crate::ui::PaneFocusColors {
            accent: ACCENT,
            overlay0: OVERLAY0,
            overlay1: Color::Rgb(90, 90, 90),
            surface_dim: Color::Rgb(20, 20, 20),
        },
        true,
    )
}

fn two_pane_snapshot(focused: &str, revision: u64) -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    snapshot.revision = revision;
    snapshot.focused_pane_id = Some(focused.into());
    let mut second = snapshot.panes[0].clone();
    second.pane_id = "pane_2".into();
    snapshot.panes.push(second);
    for pane in &mut snapshot.panes {
        pane.focused = pane.pane_id == focused;
    }
    snapshot.pane_focus_style = Some(focus_style());
    snapshot
}

/// Two side-by-side bordered panes drawn the way the endpoint draws focus chrome.
fn two_pane_surface(focused: &str, revision: u64) -> PaneSurfaceFrame {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 4));
    let mut panes = Vec::new();
    let mut cursor = None;
    for (index, (pane_id, text)) in [("pane_1", "LEFT"), ("pane_2", "RIGHT")]
        .into_iter()
        .enumerate()
    {
        let rect = Rect::new(index as u16 * 10, 0, 10, 4);
        let inner = Rect::new(rect.x + 1, 1, 8, 2);
        let is_focused = pane_id == focused;
        let color = if is_focused { ACCENT } else { OVERLAY0 };
        ratatui::widgets::Widget::render(
            ratatui::widgets::Block::bordered().border_style(Style::default().fg(color)),
            rect,
            &mut buffer,
        );
        buffer.set_string(inner.x, inner.y, text, Style::default());
        if is_focused {
            cursor = Some(crate::protocol::CursorState {
                x: inner.x + text.len() as u16,
                y: inner.y,
                visible: true,
                shape: 0,
            });
        }
        panes.push(PaneSurfacePane {
            pane_id: pane_id.into(),
            content_revision: 0,
            rect: rect.into(),
            inner_rect: inner.into(),
            scrollbar_rect: None,
            scroll: None,
            focused: is_focused,
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            alternate_screen_active: false,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
    PaneSurfaceFrame {
        boot_id: "boot-1".into(),
        projection_revision: revision,
        surface_revision: revision,
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, cursor, &[]),
        panes,
        splits: Vec::new(),
        popup: None,
        graphics: crate::protocol::SurfaceGraphicsScene::default(),
    }
}

fn presented_state() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(two_pane_snapshot("pane_1", 1)));
    state.set_pane_surface(two_pane_surface("pane_1", 1));
    state
}

/// The pane whose left border the composed frame draws in the focus color.
fn composed_focus(state: &mut ClientShellState) -> (FrameData, Vec<&'static str>) {
    let frame = state.compose(100, 28).expect("composed frame").frame;
    let origin = state.layout(100, 28).pane_surface;
    let accent = crate::protocol::color_to_u32(ACCENT);
    let focused = [("pane_1", 0), ("pane_2", 10)]
        .into_iter()
        .filter(|(_, x)| {
            let index =
                usize::from(origin.y + 1) * usize::from(frame.width) + usize::from(origin.x + x);
            frame.cells[index].fg == accent
        })
        .map(|(pane_id, _)| pane_id)
        .collect();
    (frame, focused)
}

fn request_id(actions: &[ClientShellAction]) -> &str {
    let [ClientShellAction::Endpoint { request, .. }] = actions else {
        panic!("expected one endpoint request");
    };
    &request.id
}

struct AcceptingTransport;

impl crate::client::endpoint::EndpointTransport for AcceptingTransport {
    fn send(&mut self, _: &ClientMessage) -> std::io::Result<()> {
        Ok(())
    }
}

fn focus_right(state: &mut ClientShellState) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::FocusPaneRight),
        &mut outcome,
    );
    outcome
}

fn typed_pane(state: &mut ClientShellState) -> String {
    let typed = state.handle_input_bytes(b"x");
    let [ClientMessage::ClientShellPaneInput { pane_id, .. }] = &typed.requests[..] else {
        panic!("expected pane input: {:?}", typed.requests);
    };
    pane_id.clone()
}

#[test]
fn pane_focus_shows_before_the_reply_and_takes_keys() {
    let mut state = presented_state();
    assert_eq!(composed_focus(&mut state).1, ["pane_1"]);

    let outcome = focus_right(&mut state);
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("focus should use one endpoint request");
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::PaneFocus(target) if target.pane_id == "pane_2"
    ));
    assert!(outcome.repaint);

    let (frame, focused) = composed_focus(&mut state);
    assert_eq!(focused, ["pane_2"]);
    assert!(
        frame.cursor.is_none(),
        "the old pane's cursor must not stay"
    );
    assert!(frame_rows(&frame).iter().any(|row| row.contains("LEFT")));
    assert_eq!(typed_pane(&mut state), "pane_2");
}

#[test]
fn confirmed_pane_focus_repaints_the_same_cells() {
    for reply_first in [true, false] {
        let mut state = presented_state();
        let outcome = focus_right(&mut state);
        let id = request_id(&outcome.actions).to_owned();
        let (predicted, _) = composed_focus(&mut state);

        if reply_first {
            state.handle_endpoint_result("boot-1", &id, Ok(pane_scroll_result(0, 0, 2)));
            assert_eq!(composed_focus(&mut state).1, ["pane_2"]);
        }
        state.set_snapshot(Box::new(two_pane_snapshot("pane_2", 2)));
        assert!(
            state.compose(100, 28).is_none(),
            "no frame may show the old surface under the new snapshot"
        );
        state.set_pane_surface(two_pane_surface("pane_2", 2));
        if !reply_first {
            state.handle_endpoint_result("boot-1", &id, Ok(pane_scroll_result(0, 0, 2)));
        }

        assert!(state.predicted_pane_focus.is_none());
        let (confirmed, focused) = composed_focus(&mut state);
        assert_eq!(focused, ["pane_2"]);
        assert_eq!(
            confirmed.cells, predicted.cells,
            "reply_first={reply_first}"
        );
        assert!(confirmed.cursor.is_some());
        assert_eq!(typed_pane(&mut state), "pane_2");
    }
}

#[test]
fn rejected_pane_focus_returns_to_the_endpoint_focus() {
    let mut state = presented_state();
    let outcome = focus_right(&mut state);
    let id = request_id(&outcome.actions).to_owned();
    let (repaint, _) = state.handle_endpoint_result(
        "boot-1",
        &id,
        Err(ClientShellEndpointError {
            code: Some("pane_not_found".into()),
            message: "pane not found".into(),
        }),
    );
    assert!(repaint);
    assert_eq!(composed_focus(&mut state).1, ["pane_1"]);
    assert_eq!(typed_pane(&mut state), "pane_1");

    // An accepted request whose snapshot shows another pane also yields to the endpoint.
    let outcome = focus_right(&mut state);
    let id = request_id(&outcome.actions).to_owned();
    state.handle_endpoint_result("boot-1", &id, Ok(pane_scroll_result(0, 0, 2)));
    assert_eq!(composed_focus(&mut state).1, ["pane_2"]);
    state.set_snapshot(Box::new(two_pane_snapshot("pane_1", 2)));
    state.set_pane_surface(two_pane_surface("pane_1", 2));
    assert_eq!(composed_focus(&mut state).1, ["pane_1"]);
    assert_eq!(typed_pane(&mut state), "pane_1");
}

#[test]
fn rapid_pane_focus_sends_only_the_latest_queued_target() {
    use crate::client::endpoint::{EndpointNegotiation, EndpointRegistry};
    use crate::client::endpoint_commands::EndpointCommands;

    let mut state = presented_state();
    let mut endpoints =
        EndpointRegistry::new(AcceptingTransport, 1, EndpointNegotiation::default());
    endpoints.set_surface_active(&ClientEndpointId::Local, true);
    let mut commands = EndpointCommands::default();
    let mut request_ids = Vec::new();
    for action in [
        crate::input::KeybindAction::FocusPaneRight,
        crate::input::KeybindAction::FocusPaneLeft,
        crate::input::KeybindAction::FocusPaneRight,
        crate::input::KeybindAction::FocusPaneLeft,
    ] {
        let mut outcome = ClientShellInput::default();
        state.record_binding(crate::input::KeybindMatch::Action(action), &mut outcome);
        request_ids.push(request_id(&outcome.actions).to_owned());
        crate::client::shell_runtime::dispatch_client_shell_actions(
            outcome.actions,
            &mut commands,
            &mut endpoints,
            Some(&mut state),
            &mut Vec::new(),
            &mut None,
        )
        .unwrap();
    }
    assert_eq!(composed_focus(&mut state).1, ["pane_1"]);
    assert_eq!(
        state.pending_requests.keys().collect::<HashSet<_>>(),
        HashSet::from([&request_ids[0], &request_ids[3]]),
        "superseded focus requests are dropped without a notice"
    );
    assert!(state.visible_endpoint_notice.is_none());

    // The endpoint confirms the first target while the latest is still queued.
    state.handle_endpoint_result("boot-1", &request_ids[0], Ok(pane_scroll_result(0, 0, 2)));
    state.set_snapshot(Box::new(two_pane_snapshot("pane_2", 2)));
    state.set_pane_surface(two_pane_surface("pane_2", 2));
    assert_eq!(composed_focus(&mut state).1, ["pane_1"]);
    assert_eq!(typed_pane(&mut state), "pane_1");
    assert_eq!(
        commands.retire_lane(&ClientEndpointId::Local),
        [request_ids[0].clone(), request_ids[3].clone()]
    );
}

#[test]
fn pane_focus_without_endpoint_style_waits_for_the_endpoint() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let mut snapshot = two_pane_snapshot("pane_1", 1);
    snapshot.pane_focus_style = None;
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(two_pane_surface("pane_1", 1));

    let outcome = focus_right(&mut state);
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("focus should use one endpoint request");
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::PaneFocusDirection(_)
    ));
    assert_eq!(composed_focus(&mut state).1, ["pane_1"]);
    assert_eq!(typed_pane(&mut state), "pane_1");
}

const JAN_BOX_FOCUS: Color = Color::Rgb(0x99, 0xff, 0xe4);

fn machine_focus_config(entries: &[(&str, &str)]) -> Config {
    let mut config = Config::default();
    config.ui.machine_focus_colors = entries
        .iter()
        .map(|(label, color)| ((*label).to_owned(), (*color).to_owned()))
        .collect();
    config
}

/// `two_pane_surface` with a " T " title on pane_1's top border and an accent-colored
/// terminal cell inside pane_1.
fn titled_surface(focused: &str, revision: u64) -> PaneSurfaceFrame {
    let mut surface = two_pane_surface(focused, revision);
    let border = crate::protocol::color_to_u32(if focused == "pane_1" {
        ACCENT
    } else {
        OVERLAY0
    });
    for (x, symbol) in [(1, " "), (2, "T"), (3, " ")] {
        let cell = &mut surface.frame.cells[x];
        cell.symbol = symbol.into();
        cell.fg = border;
    }
    surface.frame.cells[usize::from(surface.frame.width) + 1].fg =
        crate::protocol::color_to_u32(ACCENT);
    surface
}

/// A state presenting `titled_surface` from the saved machine "jan-box".
fn jan_box_state(config: &Config) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(config));
    let profile = SavedSshEndpoint {
        id: crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "jan-box".into(),
        target: "jan-box".into(),
        session: "default".into(),
        enabled: true,
    };
    let endpoint = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&endpoint, ClientEndpointStatus::Online);
    state.set_endpoint_snapshot(&endpoint, Box::new(two_pane_snapshot("pane_1", 1)));
    assert!(state.activate_endpoint_projection(&endpoint));
    state.set_pane_surface(titled_surface("pane_1", 1));
    assert_eq!(state.active_endpoint_label(), "jan-box");
    state
}

/// Foreground colors of surface cells (`x`, `y`) in a composed frame.
fn surface_fg(state: &ClientShellState, frame: &FrameData, cells: &[(u16, u16)]) -> Vec<u32> {
    let origin = state.layout(100, 28).pane_surface;
    cells
        .iter()
        .map(|(x, y)| {
            frame.cells
                [usize::from(origin.y + y) * usize::from(frame.width) + usize::from(origin.x + x)]
            .fg
        })
        .collect()
}

const PANE_1_BORDER: [(u16, u16); 4] = [(0, 0), (0, 1), (9, 2), (5, 3)];
const PANE_1_TITLE: [(u16, u16); 3] = [(1, 0), (2, 0), (3, 0)];
const PANE_2_BORDER: [(u16, u16); 4] = [(10, 0), (10, 1), (19, 2), (15, 3)];
const PANE_1_CONTENT: (u16, u16) = (1, 1);

fn all(color: Color, count: usize) -> Vec<u32> {
    vec![crate::protocol::color_to_u32(color); count]
}

#[test]
fn machine_focus_color_presents_the_focused_border_of_that_machine() {
    let mut state = jan_box_state(&machine_focus_config(&[("jan-box", "#99ffe4")]));
    let frame = state.compose(100, 28).expect("composed frame").frame;

    assert_eq!(
        surface_fg(&state, &frame, &PANE_1_BORDER),
        all(JAN_BOX_FOCUS, 4)
    );
    assert_eq!(
        surface_fg(&state, &frame, &PANE_1_TITLE),
        all(JAN_BOX_FOCUS, 3)
    );
    assert_eq!(surface_fg(&state, &frame, &PANE_2_BORDER), all(OVERLAY0, 4));
    assert_eq!(
        surface_fg(&state, &frame, &[PANE_1_CONTENT]),
        all(ACCENT, 1),
        "terminal cells keep their own colors"
    );
    assert!(frame.cursor.is_some());
    assert_eq!(
        state.pane_surface.as_ref().unwrap().frame,
        titled_surface("pane_1", 1).frame,
        "the stored surface stays as the endpoint sent it"
    );

    focus_right(&mut state);
    let frame = state.compose(100, 28).expect("predicted frame").frame;
    assert_eq!(
        surface_fg(&state, &frame, &PANE_2_BORDER),
        all(JAN_BOX_FOCUS, 4)
    );
    assert_eq!(surface_fg(&state, &frame, &PANE_1_BORDER), all(OVERLAY0, 4));
    assert_eq!(surface_fg(&state, &frame, &PANE_1_TITLE), all(OVERLAY0, 3));
    assert_eq!(
        surface_fg(&state, &frame, &[PANE_1_CONTENT]),
        all(ACCENT, 1)
    );
}

#[test]
fn machine_focus_color_survives_fast_surface_patches() {
    let mut state = jan_box_state(&machine_focus_config(&[("jan-box", "#99ffe4")]));
    let composed = state.compose(100, 28).expect("composed frame");
    let mut pane = state.pane_surface.as_ref().unwrap().panes[0].clone();
    pane.content_revision = 2;
    let mut cell = composed.cells[0].clone();
    cell.symbol = "N".into();
    cell.fg = crate::protocol::color_to_u32(ACCENT);
    let patch = crate::protocol::PaneSurfacePatch {
        boot_id: "boot-1".into(),
        projection_revision: 1,
        base_surface_revision: 1,
        surface_revision: 2,
        rows: vec![crate::protocol::PaneSurfacePatchRow {
            x: 1,
            y: 2,
            cells: vec![cell; 8],
        }],
        panes: vec![pane],
        cursor: None,
    };

    let ClientPaneSurfacePatchOutcome::Applied(Some(patch)) = state.apply_pane_surface_patch(patch)
    else {
        panic!("expected a fast surface patch");
    };
    let patched =
        super::super::apply_composed_surface_patch(&composed, patch).expect("apply composed patch");

    assert_eq!(
        surface_fg(&state, &patched, &PANE_1_BORDER),
        all(JAN_BOX_FOCUS, 4)
    );
    assert_eq!(
        surface_fg(&state, &patched, &PANE_1_TITLE),
        all(JAN_BOX_FOCUS, 3)
    );
    assert_eq!(
        surface_fg(&state, &patched, &[(1, 2), (8, 2)]),
        all(ACCENT, 2)
    );
    let recomposed = state.compose(100, 28).expect("recomposed frame");
    assert_eq!(
        patched.cells, recomposed.cells,
        "the fast path presents what a full compose presents"
    );
}

#[test]
fn machine_focus_color_leaves_machines_without_an_entry_unchanged() {
    let compose_local = |config: &Config| {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(config));
        state.set_snapshot(Box::new(two_pane_snapshot("pane_1", 1)));
        state.set_pane_surface(titled_surface("pane_1", 1));
        assert_eq!(state.active_endpoint_label(), "Local");
        state.compose(100, 28).expect("composed frame").frame
    };

    let today = compose_local(&Config::default());
    let configured = compose_local(&machine_focus_config(&[
        ("jan-box", "#99ffe4"),
        ("local", "magenta"),
    ]));

    assert_eq!(configured, today);
}

#[test]
fn machine_focus_color_follows_live_config_reload() {
    let mut state = jan_box_state(&Config::default());
    let frame = state.compose(100, 28).expect("composed frame").frame;
    assert_eq!(surface_fg(&state, &frame, &PANE_1_BORDER), all(ACCENT, 4));

    for (entries, expected) in [
        (&[("jan-box", "#99ffe4")][..], JAN_BOX_FOCUS),
        (&[("jan-box", "magenta")][..], Color::Magenta),
        (&[][..], ACCENT),
    ] {
        state
            .config
            .apply_live_config(&machine_focus_config(entries), &[], &[]);
        let frame = state.compose(100, 28).expect("composed frame").frame;
        assert_eq!(
            surface_fg(&state, &frame, &PANE_1_BORDER),
            all(expected, 4),
            "{entries:?}"
        );
    }
}
