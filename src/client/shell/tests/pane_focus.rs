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
