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
    let [(header, key), (dotfiles_header, dotfiles_key)] = &state.hits.projects[..] else {
        panic!("devkit and dotfiles are projects");
    };
    assert_eq!(key, DEVKIT);
    assert!(rows[header.y as usize].contains("ws_1"));
    assert_eq!(dotfiles_key, DOTFILES);
    assert!(rows[dotfiles_header.y as usize].contains("ws_2"));

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

fn renamed(mut workspace: ClientShellWorkspace, label: &str, branch: &str) -> ClientShellWorkspace {
    workspace.label = label.into();
    workspace.custom_label = true;
    workspace.branch = Some(branch.into());
    workspace
}

/// Text of the sidebar row that holds the space, found through its hit rectangle.
fn row_text(frame: &FrameData, state: &ClientShellState, id: &str) -> String {
    let hit = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.workspace_id == id)
        .expect("space is visible");
    frame_rows(frame)[hit.rect.y as usize].clone()
}

#[test]
fn project_header_takes_the_main_label_and_the_main_row_shows_its_branch() {
    let (mut state, remote_id) = project_state(SidebarGroupBy::Project);
    let mut local = snapshot();
    local.workspaces = vec![renamed(
        workspace("ws_1", Some((DEVKIT, "devkit", false)), true),
        "DEVKIT-MAIN",
        "trunk",
    )];
    state.set_snapshot(Box::new(local));
    let mut remote = snapshot();
    remote.boot_id = "remote-boot".into();
    remote.focused_workspace_id = Some("rws_1".into());
    remote.workspaces = vec![renamed(
        workspace("rws_1", Some((DEVKIT, "devkit", true)), true),
        "MY-WORKTREE",
        "worktree/feature",
    )];
    state.set_endpoint_snapshot(&remote_id, Box::new(remote));
    let frame = state.compose(100, 40).expect("project sidebar");
    let rows = frame_rows(&frame);

    let [(header, _)] = &state.hits.projects[..] else {
        panic!("devkit has two spaces");
    };
    assert!(rows[header.y as usize].contains("DEVKIT-MAIN"));
    let main = row_text(&frame, &state, "ws_1");
    assert!(main.contains("trunk") && !main.contains("DEVKIT-MAIN"));
    let linked = row_text(&frame, &state, "rws_1");
    assert!(linked.contains("MY-WORKTREE") && !linked.contains("feature"));
}

#[test]
fn one_workspace_project_gets_a_header_and_an_indented_branch_row() {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.sidebar_group_by = SidebarGroupBy::Project;
    let mut state = ClientShellState::new(config);
    let mut local = snapshot();
    local.workspaces = vec![
        renamed(
            workspace("ws_1", Some((DOTFILES, "dotfiles", false)), true),
            "HOME-DOTS",
            "trunk",
        ),
        workspace("ws_2", None, false),
    ];
    state.set_snapshot(Box::new(local));
    state.set_pane_surface(surface());
    let frame = state.compose(100, 40).expect("project sidebar");
    let [(header, key)] = &state.hits.projects[..] else {
        panic!("dotfiles is a project with one space");
    };
    assert_eq!(key, DOTFILES);
    let header = *header;
    assert!(frame_rows(&frame)[header.y as usize].contains("HOME-DOTS"));
    let [project_row, loose_row] = &state.hits.workspaces[..] else {
        panic!("two spaces are visible");
    };
    assert!(project_row.indented && !loose_row.indented);
    assert!(project_row.rect.y > header.y);
    let text = row_text(&frame, &state, "ws_1");
    assert!(text.contains("trunk") && !text.contains("HOME-DOTS"));

    click(&mut state, header);
    state.compose(100, 40).expect("collapsed project");
    assert_eq!(visible_workspaces(&state), ["ws_1", "ws_2"]);
}

#[test]
fn project_without_a_main_checkout_keeps_the_repository_name() {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.sidebar_group_by = SidebarGroupBy::Project;
    let mut state = ClientShellState::new(config);
    let mut local = snapshot();
    local.workspaces = vec![
        workspace("ws_1", Some((DEVKIT, "devkit", true)), true),
        workspace("ws_2", Some((DEVKIT, "devkit", true)), false),
    ];
    state.set_snapshot(Box::new(local));
    state.set_pane_surface(surface());
    let frame = state.compose(100, 40).expect("project sidebar");
    let [(header, _)] = &state.hits.projects[..] else {
        panic!("devkit has two spaces");
    };
    assert!(frame_rows(&frame)[header.y as usize].contains("devkit"));
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

const DEVKIT_ROOT: &str = "/Users/andrej/dev-work/arx1/devkit";

fn right_click(state: &mut ClientShellState, rect: Rect) {
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: rect.x + 3,
        row: rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
}

fn menu_labels(state: &ClientShellState) -> Vec<String> {
    match &state.overlay {
        Some(ClientShellOverlay::ContextMenu(menu)) => {
            menu.items().into_iter().map(|item| item.label).collect()
        }
        _ => panic!("a menu is open"),
    }
}

fn pick(state: &mut ClientShellState, label: &str) -> ClientShellInput {
    let index = menu_labels(state)
        .iter()
        .position(|item| item == label)
        .unwrap_or_else(|| panic!("menu offers {label}"));
    let mut outcome = ClientShellInput::default();
    state.activate_context_menu_item(index, &mut outcome);
    outcome
}

fn open_project_menu(state: &mut ClientShellState, key: &str) {
    state.compose(100, 40).expect("project sidebar");
    let header = state
        .hits
        .projects
        .iter()
        .find(|(_, project)| project == key)
        .expect("project header")
        .0;
    right_click(state, header);
}

fn header_row(state: &mut ClientShellState, key: &str) -> String {
    let frame = state.compose(100, 40).expect("project sidebar");
    let header = state
        .hits
        .projects
        .iter()
        .find(|(_, project)| project == key)
        .expect("project header")
        .0;
    frame_rows(&frame)[header.y as usize].clone()
}

fn press_enter(state: &mut ClientShellState) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    ))])
}

fn set_rename_input(state: &mut ClientShellState, text: &str) {
    let Some(ClientShellOverlay::Rename(rename)) = state.overlay.as_mut() else {
        panic!("rename overlay is open");
    };
    rename.input = TextEditor::new(text, false);
}

/// The one endpoint request in `actions`, with the endpoint it goes to.
fn endpoint_request(
    actions: &[ClientShellAction],
) -> (&ClientEndpointId, &crate::api::schema::Request) {
    let [ClientShellAction::Endpoint {
        endpoint_id,
        request,
        ..
    }] = actions
    else {
        panic!("expected one endpoint request: {actions:?}");
    };
    (endpoint_id, request)
}

fn temp_preferences(state: &mut ClientShellState, name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("herdr-{name}-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    state.config.preferences_path = Some(path.clone());
    path
}

#[test]
fn project_names_round_trip_through_preferences() {
    let (mut state, _) = project_state(SidebarGroupBy::Project);
    let path = temp_preferences(&mut state, "project-names");
    let mut outcome = ClientShellInput::default();
    state.persist_chrome_preferences(&mut outcome);
    let saved = std::fs::read_to_string(&path).expect("saved preferences");
    assert!(!saved.contains("project_names"), "no names, no field");

    state.project_names.insert(DEVKIT.into(), "Devkit".into());
    state.persist_chrome_preferences(&mut outcome);
    let loaded = preferences::load(&path).expect("saved preferences");
    assert_eq!(
        loaded.project_names,
        BTreeMap::from([(DEVKIT.to_owned(), "Devkit".to_owned())])
    );
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.preferences = loaded;
    assert_eq!(
        ClientShellState::new(config).project_names,
        state.project_names
    );
    std::fs::remove_file(path).expect("remove preferences");
}

#[test]
fn saved_project_name_heads_the_project_before_the_main_label() {
    let (mut state, _) = project_state(SidebarGroupBy::Project);
    assert!(header_row(&mut state, DEVKIT).contains("ws_1"));
    state.project_names.insert(DEVKIT.into(), "Devkit".into());
    let row = header_row(&mut state, DEVKIT);
    assert!(row.contains("Devkit") && !row.contains("ws_1"));
    // Other projects keep their derived label.
    assert!(header_row(&mut state, DOTFILES).contains("ws_2"));
}

#[test]
fn rename_project_saves_the_name_and_an_empty_name_clears_it() {
    let (mut state, _) = project_state(SidebarGroupBy::Project);
    let path = temp_preferences(&mut state, "project-rename");
    open_project_menu(&mut state, DEVKIT);
    pick(&mut state, "Rename");
    let Some(ClientShellOverlay::Rename(rename)) = &state.overlay else {
        panic!("rename overlay is open");
    };
    assert_eq!(rename.title, "rename project");
    assert_eq!(rename.input.as_str(), "ws_1");

    set_rename_input(&mut state, "  Devkit ");
    let saved = press_enter(&mut state);
    assert!(saved.actions.is_empty(), "renaming stays in the client");
    assert!(saved.repaint);
    assert_eq!(
        state.project_names.get(DEVKIT).map(String::as_str),
        Some("Devkit")
    );
    assert_eq!(
        preferences::load(&path).map(|saved| saved.project_names),
        Some(BTreeMap::from([(DEVKIT.to_owned(), "Devkit".to_owned())]))
    );
    assert!(header_row(&mut state, DEVKIT).contains("Devkit"));

    open_project_menu(&mut state, DEVKIT);
    pick(&mut state, "Rename");
    let Some(ClientShellOverlay::Rename(rename)) = &state.overlay else {
        panic!("rename overlay is open");
    };
    assert_eq!(rename.input.as_str(), "Devkit");
    set_rename_input(&mut state, " ");
    assert!(press_enter(&mut state).actions.is_empty());
    assert!(state.project_names.is_empty());
    assert!(!std::fs::read_to_string(&path)
        .expect("saved preferences")
        .contains("project_names"));
    assert!(header_row(&mut state, DEVKIT).contains("ws_1"));
    std::fs::remove_file(path).expect("remove preferences");
}

#[test]
fn project_menu_offers_a_worktree_on_each_online_machine() {
    let (mut state, remote_id) = project_state(SidebarGroupBy::Project);
    open_project_menu(&mut state, DEVKIT);
    assert_eq!(
        menu_labels(&state),
        [
            "Rename",
            "New worktree on Local",
            "New worktree on Build",
            "Collapse"
        ]
    );

    pick(&mut state, "Collapse");
    assert!(state.collapsed_projects.contains(DEVKIT));
    open_project_menu(&mut state, DEVKIT);
    assert_eq!(
        menu_labels(&state).last().map(String::as_str),
        Some("Expand")
    );

    state.overlay = None;
    state.set_endpoint_status(&remote_id, ClientEndpointStatus::Reconnecting);
    open_project_menu(&mut state, DEVKIT);
    assert_eq!(
        menu_labels(&state),
        ["Rename", "New worktree on Local", "Expand"]
    );
}

#[test]
fn project_whose_key_is_not_a_git_directory_offers_no_worktree() {
    let (mut state, _) = project_state(SidebarGroupBy::Project);
    let bare = "/srv/git/devkit.git";
    let mut local = snapshot();
    local.workspaces = vec![workspace("ws_1", Some((bare, "devkit", true)), true)];
    state.set_snapshot(Box::new(local));
    open_project_menu(&mut state, bare);
    assert_eq!(menu_labels(&state), ["Rename", "Collapse"]);
}

#[test]
fn new_worktree_on_the_active_machine_lists_from_the_checkout_root() {
    let (mut state, _) = project_state(SidebarGroupBy::Project);
    open_project_menu(&mut state, DEVKIT);
    let outcome = pick(&mut state, "New worktree on Local");
    let (endpoint_id, request) = endpoint_request(&outcome.actions);
    assert_eq!(endpoint_id, &ClientEndpointId::Local);
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::WorktreeList(params)
            if params.cwd.as_deref() == Some(DEVKIT_ROOT) && params.workspace_id.is_none()
    ));
}

#[test]
fn new_worktree_on_another_machine_switches_to_it_and_creates_there() {
    let (mut state, remote_id) = project_state(SidebarGroupBy::Project);
    open_project_menu(&mut state, DEVKIT);
    let outcome = pick(&mut state, "New worktree on Build");
    assert!(matches!(
        &outcome.actions[..],
        [ClientShellAction::ActivateEndpoint { endpoint_id, target: None }]
            if endpoint_id == &remote_id
    ));

    // The runtime runs the waiting action once the switch commits.
    assert!(state.activate_endpoint_projection(&remote_id));
    let actions = state.start_pending_endpoint_intent();
    let (endpoint_id, request) = endpoint_request(&actions);
    assert_eq!(endpoint_id, &remote_id);
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::WorktreeList(params)
            if params.cwd.as_deref() == Some(DEVKIT_ROOT) && params.workspace_id.is_none()
    ));
    let list_id = request.id.clone();
    state.handle_endpoint_result("remote-boot", &list_id, Ok(worktree_list_result(None)));
    assert!(matches!(
        &state.overlay,
        Some(ClientShellOverlay::WorktreeCreate(create))
            if create.source == ClientWorktreeSource::Checkout(DEVKIT_ROOT.into())
    ));

    let submitted = press_enter(&mut state);
    let (endpoint_id, request) = endpoint_request(&submitted.actions);
    assert_eq!(endpoint_id, &remote_id);
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::WorktreeCreate(params)
            if params.cwd.as_deref() == Some(DEVKIT_ROOT) && params.workspace_id.is_none()
    ));
    let create_id = request.id.clone();
    let failed = state.handle_endpoint_result(
        "remote-boot",
        &create_id,
        Err(ClientShellEndpointError {
            code: Some("not_git_worktree".into()),
            message: "no repository here".into(),
        }),
    );
    assert!(failed.1.is_empty());
    assert!(matches!(
        &state.overlay,
        Some(ClientShellOverlay::WorktreeCreate(create))
            if !create.creating && create.error.as_deref() == Some("no repository here")
    ));

    let retried = press_enter(&mut state);
    let create_id = endpoint_request(&retried.actions).1.id.clone();
    let (_, focus) = state.handle_endpoint_result(
        "remote-boot",
        &create_id,
        Ok(super::endpoint_requests::worktree_created_result()),
    );
    let (endpoint_id, request) = endpoint_request(&focus);
    assert_eq!(endpoint_id, &remote_id);
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::TabFocus(target) if target.tab_id == "tab_2"
    ));
    assert!(state.overlay.is_none());
}

#[test]
fn waiting_action_is_dropped_when_the_switch_ends_elsewhere() {
    let (mut state, remote_id) = project_state(SidebarGroupBy::Project);
    open_project_menu(&mut state, DEVKIT);
    pick(&mut state, "New worktree on Build");
    // The switch rolled back, so Local is still active.
    assert!(state.start_pending_endpoint_intent().is_empty());
    assert!(state.pending_endpoint_intent.is_none());

    pick_new_workspace_on(&mut state, "Build");
    // A later selection replaces the waiting action.
    state.compose(100, 40).expect("project sidebar");
    let local_row = state.hits.workspaces[0].rect;
    click(&mut state, local_row);
    assert!(state.pending_endpoint_intent.is_none());
    assert!(state.activate_endpoint_projection(&remote_id));
    assert!(state.start_pending_endpoint_intent().is_empty());
}

fn footer_text(state: &mut ClientShellState) -> String {
    let frame = state.compose(100, 40).expect("sidebar");
    let footer = state.hits.new_workspace;
    frame_rows(&frame)[footer.y as usize]
        .chars()
        .take(footer.width as usize)
        .collect()
}

fn pick_new_workspace_on(state: &mut ClientShellState, machine: &str) -> ClientShellInput {
    state.compose(100, 40).expect("sidebar");
    let footer = state.hits.new_workspace;
    let opened = click(state, footer);
    assert!(opened.actions.is_empty());
    pick(state, &format!("New workspace on {machine}"))
}

#[test]
fn footer_asks_which_machine_when_several_are_online() {
    let (mut state, remote_id) = project_state(SidebarGroupBy::Project);
    assert_eq!(footer_text(&mut state), " new ▾");
    let footer = state.hits.new_workspace;
    click(&mut state, footer);
    assert_eq!(
        menu_labels(&state),
        ["New workspace on Local", "New workspace on Build"]
    );
    state.overlay = None;

    let outcome = pick_new_workspace_on(&mut state, "Build");
    assert!(matches!(
        &outcome.actions[..],
        [ClientShellAction::ActivateEndpoint { endpoint_id, target: None }]
            if endpoint_id == &remote_id
    ));
    assert!(state.activate_endpoint_projection(&remote_id));
    let actions = state.start_pending_endpoint_intent();
    let (endpoint_id, request) = endpoint_request(&actions);
    assert_eq!(endpoint_id, &remote_id);
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::WorkspaceCreate(params)
            if params.focus && params.source_workspace_id.as_deref() == Some("rws_1")
    ));
}

#[test]
fn footer_pick_prompts_for_the_name_on_the_chosen_machine() {
    let (mut state, remote_id) = project_state(SidebarGroupBy::Project);
    state.config.prompt_new_workspace_name = true;
    let outcome = pick_new_workspace_on(&mut state, "Local");
    assert!(outcome.actions.is_empty());
    assert!(matches!(
        &state.overlay,
        Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            target: ClientRenameTarget::NewWorkspace { source_workspace_id: Some(id), .. },
            ..
        })) if id == "ws_1"
    ));

    state.overlay = None;
    pick_new_workspace_on(&mut state, "Build");
    assert!(state.activate_endpoint_projection(&remote_id));
    assert!(state.start_pending_endpoint_intent().is_empty());
    assert!(matches!(
        &state.overlay,
        Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            target: ClientRenameTarget::NewWorkspace { source_workspace_id: Some(id), .. },
            ..
        })) if id == "rws_1"
    ));
}

#[test]
fn footer_creates_at_once_with_one_machine_online() {
    let (mut state, remote_id) = project_state(SidebarGroupBy::Project);
    state.set_endpoint_status(&remote_id, ClientEndpointStatus::Reconnecting);
    assert_eq!(footer_text(&mut state), " new");
    let footer = state.hits.new_workspace;
    let outcome = click(&mut state, footer);
    assert!(state.overlay.is_none());
    let (endpoint_id, request) = endpoint_request(&outcome.actions);
    assert_eq!(endpoint_id, &ClientEndpointId::Local);
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::WorkspaceCreate(params) if params.focus
    ));
}

#[test]
fn machine_grouping_keeps_its_footer_and_menus() {
    let (mut state, _) = project_state(SidebarGroupBy::default());
    assert_eq!(footer_text(&mut state), " new · Local");
    let footer = state.hits.new_workspace;
    let outcome = click(&mut state, footer);
    assert!(state.overlay.is_none());
    let (endpoint_id, request) = endpoint_request(&outcome.actions);
    assert_eq!(endpoint_id, &ClientEndpointId::Local);
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::WorkspaceCreate(_)
    ));

    state.compose(100, 40).expect("machine sidebar");
    let row = state.hits.workspaces[0].rect;
    right_click(&mut state, row);
    assert!(matches!(
        &state.overlay,
        Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Workspace { .. },
            ..
        }))
    ));
}
