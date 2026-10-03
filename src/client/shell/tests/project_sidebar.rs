use super::*;
use crate::client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, ProfileId, SavedSshEndpoint,
};
use crate::config::SidebarGroupBy;
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

const DEVKIT: &str = "/Users/andrej/dev-work/arx1/devkit/.git";
const DOTFILES: &str = "/Users/andrej/dotfiles/.git";

fn workspace(id: &str, project: Option<(&str, &str, bool)>, focused: bool) -> ClientShellWorkspace {
    ClientShellWorkspace {
        workspace_id: id.into(),
        label: id.into(),
        focused,
        worktree: project.map(|(key, label, linked)| ClientShellWorktree {
            key: key.into(),
            label: label.into(),
            is_linked_worktree: linked,
        }),
        ..snapshot().workspaces[0].clone()
    }
}

/// Local: devkit (focused), dotfiles, scratch. Build: devkit's `mutants` worktree.
fn project_state(group_by: SidebarGroupBy) -> (ClientShellState, ClientEndpointId) {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.sidebar_group_by = group_by;
    config
        .machine_focus_colors
        .insert("Local".into(), ratatui::style::Color::Magenta);
    let mut state = ClientShellState::new(config);
    let profile = SavedSshEndpoint {
        id: ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "Build".into(),
        target: "dev@build.example".into(),
        session: "agents".into(),
        enabled: true,
    };
    let remote_id = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&remote_id, ClientEndpointStatus::Online);
    let mut local = snapshot();
    local.workspaces = vec![
        workspace("ws_1", Some((DEVKIT, "devkit", false)), true),
        workspace("ws_2", Some((DOTFILES, "dotfiles", false)), false),
        workspace("ws_3", None, false),
    ];
    state.set_snapshot(Box::new(local));
    state.set_pane_surface(surface());
    let mut remote = snapshot();
    remote.boot_id = "remote-boot".into();
    remote.focused_workspace_id = Some("rws_1".into());
    remote.workspaces = vec![workspace("rws_1", Some((DEVKIT, "devkit", true)), true)];
    state.set_endpoint_snapshot(&remote_id, Box::new(remote));
    (state, remote_id)
}

fn visible_workspaces(state: &ClientShellState) -> Vec<String> {
    state
        .hits
        .workspaces
        .iter()
        .map(|hit| hit.workspace_id.clone())
        .collect()
}

fn click(state: &mut ClientShellState, rect: Rect) -> ClientShellInput {
    let at = |kind| {
        RawInputEvent::Mouse(MouseEvent {
            kind,
            column: rect.x + 3,
            row: rect.y,
            modifiers: KeyModifiers::empty(),
        })
    };
    let mut outcome = state.handle_raw_events(vec![at(MouseEventKind::Down(MouseButton::Left))]);
    let up = state.handle_raw_events(vec![at(MouseEventKind::Up(MouseButton::Left))]);
    outcome.actions.extend(up.actions);
    outcome
}

fn next_workspace(state: &mut ClientShellState) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NextWorkspace),
        &mut outcome,
    );
    outcome
}

#[test]
fn project_sidebar_groups_machines_and_marks_rows_with_machine_color() {
    let (mut state, remote_id) = project_state(SidebarGroupBy::Project);
    let frame = state.compose(100, 40).expect("project sidebar");
    let rows = frame_rows(&frame);
    assert!(rows.iter().any(|row| row.starts_with(" projects")));
    assert!(!rows.iter().any(|row| row.contains("machines")));
    assert!(state.hits.machines.is_empty());
    assert_eq!(
        visible_workspaces(&state),
        ["ws_1", "rws_1", "ws_2", "ws_3"]
    );
    let [(header, key)] = &state.hits.projects[..] else {
        panic!("only devkit has two spaces");
    };
    assert_eq!(key, DEVKIT);
    assert!(rows[header.y as usize].contains("devkit"));

    let buffer = frame.to_ratatui_buffer().expect("frame should reconstruct");
    for hit in &state.hits.workspaces {
        let cell = &buffer[(hit.rect.x, hit.rect.y)];
        if hit.endpoint_id == remote_id {
            assert_eq!(
                cell.symbol(),
                " ",
                "Build has no color, so its bar is blank"
            );
        } else {
            assert_eq!(cell.symbol(), "▌");
            assert_eq!(cell.fg, ratatui::style::Color::Magenta);
        }
    }
}

#[test]
fn machine_grouping_stays_the_default() {
    let (mut state, _) = project_state(SidebarGroupBy::default());
    let frame = state.compose(100, 40).expect("machine sidebar");
    assert!(frame_rows(&frame)
        .iter()
        .any(|row| row.starts_with(" machines")));
    assert!(state.hits.projects.is_empty());
    assert_eq!(state.hits.machines.len(), 2);
    assert_eq!(
        visible_workspaces(&state),
        ["ws_1", "ws_2", "ws_3", "rws_1"]
    );
    let next = next_workspace(&mut state);
    assert!(matches!(
        &next.actions[..],
        [ClientShellAction::ActivateEndpoint {
            endpoint_id: ClientEndpointId::Local,
            target: Some(ClientEndpointFocusTarget::Workspace(workspace_id)),
        }] if workspace_id == "ws_2"
    ));
}

#[test]
fn project_header_click_collapses_and_persists_the_project() {
    let (mut state, _) = project_state(SidebarGroupBy::Project);
    let path = std::env::temp_dir().join(format!(
        "herdr-project-sidebar-preferences-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    state.config.preferences_path = Some(path.clone());
    state.compose(100, 40).expect("expanded project");
    let header = state.hits.projects[0].0;

    click(&mut state, header);
    assert!(state.collapsed_projects.contains(DEVKIT));
    assert_eq!(
        preferences::load(&path).map(|saved| saved.collapsed_projects),
        Some(vec![DEVKIT.to_owned()])
    );
    let frame = state.compose(100, 40).expect("collapsed project");
    // The Local space is focused on the active machine, so it stays visible.
    assert_eq!(visible_workspaces(&state), ["ws_1", "ws_2", "ws_3"]);
    assert!(frame_rows(&frame)[header.y as usize].contains('▸'));
    // Machine grouping keeps its own collapse state.
    assert!(state.collapsed_groups.is_empty());

    click(&mut state, header);
    state.compose(100, 40).expect("expanded again");
    assert_eq!(
        visible_workspaces(&state),
        ["ws_1", "rws_1", "ws_2", "ws_3"]
    );
    std::fs::remove_file(path).expect("remove preferences");
}

#[test]
fn project_row_click_focuses_the_space_on_its_machine() {
    let (mut state, remote_id) = project_state(SidebarGroupBy::Project);
    state.compose(100, 40).expect("project sidebar");
    let remote_row = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.endpoint_id == remote_id)
        .expect("remote space row")
        .rect;
    let outcome = click(&mut state, remote_row);
    assert!(matches!(
        &outcome.actions[..],
        [ClientShellAction::ActivateEndpoint {
            endpoint_id,
            target: Some(ClientEndpointFocusTarget::Workspace(workspace_id)),
        }] if endpoint_id == &remote_id && workspace_id == "rws_1"
    ));
    assert!(state.chrome_drag.is_none());
}

#[test]
fn next_workspace_follows_project_order_across_machines() {
    let (mut state, remote_id) = project_state(SidebarGroupBy::Project);
    let next = next_workspace(&mut state);
    assert!(matches!(
        &next.actions[..],
        [ClientShellAction::ActivateEndpoint {
            endpoint_id,
            target: Some(ClientEndpointFocusTarget::Workspace(workspace_id)),
        }] if endpoint_id == &remote_id && workspace_id == "rws_1"
    ));
}

#[test]
fn navigate_mode_walks_visible_project_rows() {
    let (mut state, remote_id) = project_state(SidebarGroupBy::Project);
    state.compose(100, 40).expect("project sidebar");
    state.navigate_workspace_id = state.focused_navigation_target();
    state.move_navigate_workspace(1);
    assert!(state
        .navigate_workspace_id
        .as_ref()
        .is_some_and(|target| target.matches(&remote_id, "rws_1")));

    state.collapsed_projects.insert(DEVKIT.into());
    state.navigate_workspace_id = state.focused_navigation_target();
    state.move_navigate_workspace(1);
    assert!(state
        .navigate_workspace_id
        .as_ref()
        .is_some_and(|target| target.matches(&ClientEndpointId::Local, "ws_2")));
}

#[test]
fn project_grouping_applies_with_a_single_machine() {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.sidebar_group_by = SidebarGroupBy::Project;
    let mut state = ClientShellState::new(config);
    let mut local = snapshot();
    // Two linked worktrees without their main checkout: machine grouping keeps list order.
    local.workspaces = vec![
        workspace("ws_1", Some((DEVKIT, "devkit", true)), true),
        workspace("ws_2", None, false),
        workspace("ws_3", Some((DEVKIT, "devkit", true)), false),
    ];
    state.set_snapshot(Box::new(local));
    state.set_pane_surface(surface());
    let frame = state.compose(100, 40).expect("single machine projects");
    assert!(frame_rows(&frame)
        .iter()
        .any(|row| row.starts_with(" projects")));
    assert_eq!(visible_workspaces(&state), ["ws_1", "ws_3", "ws_2"]);
    let next = next_workspace(&mut state);
    assert!(matches!(
        &next.actions[..],
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(
                &request.method,
                crate::api::schema::Method::WorkspaceFocus(target) if target.workspace_id == "ws_3"
            )
    ));
}

#[test]
fn machine_that_is_not_connected_keeps_its_row() {
    let (mut state, remote_id) = project_state(SidebarGroupBy::Project);
    state.set_endpoint_status(&remote_id, ClientEndpointStatus::Reconnecting);
    let frame = state.compose(100, 40).expect("reconnecting machine");
    let [machine] = &state.hits.machines[..] else {
        panic!("only the reconnecting machine has a row");
    };
    assert_eq!(machine.endpoint_id, remote_id);
    assert_eq!(machine.collapse_toggle, Rect::default());
    assert!(frame_rows(&frame)[machine.rect.y as usize].contains("Build"));
    let first_space = state.hits.workspaces.first().expect("spaces follow").rect;
    assert!(machine.rect.y < first_space.y);
}
