use super::*;
use crate::config::CommandRunOn;

const SOURCES: [ClientShellKeybindingSource; 3] = [
    ClientShellKeybindingSource::Local,
    ClientShellKeybindingSource::RemoteLocal,
    ClientShellKeybindingSource::Endpoint,
];

fn client_config(command: &str) -> Config {
    toml::from_str(&format!(
        r#"
[[keys.command]]
key = "prefix+u"
command = "{command}"
run_on = "client"

[[keys.command]]
key = "prefix+s"
command = "server-only"
"#
    ))
    .unwrap()
}

fn server_command(id: &str, labels: &[&str]) -> crate::protocol::ClientShellCommand {
    crate::protocol::ClientShellCommand {
        command_id: id.into(),
        binding_label: labels.join(" / "),
        binding_labels: labels.iter().map(|label| (*label).into()).collect(),
        action: crate::protocol::ClientShellCommandAction::Shell,
        description: None,
    }
}

fn projection(commands: Vec<crate::protocol::ClientShellCommand>) -> ClientShellSnapshot {
    let mut projection = snapshot();
    projection.commands = commands;
    projection.server_keybindings_toml = Config::default().local_keybindings_profile_toml().ok();
    projection
}

fn state_for(
    config: &Config,
    source: ClientShellKeybindingSource,
    commands: Vec<crate::protocol::ClientShellCommand>,
) -> ClientShellState {
    let mut state = ClientShellState::new(
        ClientShellConfig::from_config(config).with_keybinding_source(source),
    );
    state.set_snapshot(Box::new(projection(commands)));
    state.set_pane_surface(surface());
    state
}

fn client_run_commands(state: &ClientShellState) -> Vec<(String, String)> {
    state
        .config
        .keybinds
        .keybinds
        .custom_commands
        .iter()
        .filter(|command| command.run_on == CommandRunOn::Client)
        .map(|command| (command.label.clone(), command.command.clone()))
        .collect()
}

fn press(state: &mut ClientShellState, code: KeyCode, modifiers: KeyModifiers) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        code, modifiers,
    ))])
}

fn press_prefixed(state: &mut ClientShellState, key: char) -> ClientShellInput {
    press(state, KeyCode::Char('b'), KeyModifiers::CONTROL);
    press(state, KeyCode::Char(key), KeyModifiers::empty())
}

fn run_of(outcome: &ClientShellInput) -> &ClientCommandRun {
    let [ClientShellAction::RunClientCommand(run)] = &outcome.actions[..] else {
        panic!("expected one client command run, got {:?}", outcome.actions);
    };
    run
}

fn env_value<'a>(run: &'a ClientCommandRun, name: &str) -> Option<&'a str> {
    run.env
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn ssh_profile() -> SavedSshEndpoint {
    SavedSshEndpoint {
        id: crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "Build box".into(),
        target: "dev@build.example".into(),
        session: "agents".into(),
        enabled: true,
    }
}

#[test]
fn client_run_command_is_registered_in_every_keybinding_source() {
    for source in SOURCES {
        let state = state_for(&client_config("open-editor"), source, Vec::new());
        assert_eq!(
            client_run_commands(&state),
            vec![("prefix+u".to_owned(), "open-editor".to_owned())],
            "{source:?}"
        );
        // The other local command stays with the server and never appears without a manifest.
        assert_eq!(
            state.config.keybinds.keybinds.custom_commands.len(),
            1,
            "{source:?}"
        );
    }
}

#[test]
fn client_run_command_survives_snapshot_manifest_refreshes() {
    for source in SOURCES {
        let mut state = state_for(
            &client_config("open-editor"),
            source,
            vec![server_command("cmd_a", &["prefix+s"])],
        );
        assert_eq!(client_run_commands(&state).len(), 1, "{source:?}");

        let mut refreshed = projection(vec![
            server_command("cmd_b", &["prefix+s"]),
            server_command("cmd_c", &["prefix+k"]),
        ]);
        refreshed.revision = 2;
        state.set_snapshot(Box::new(refreshed));
        assert_eq!(
            client_run_commands(&state),
            vec![("prefix+u".to_owned(), "open-editor".to_owned())],
            "{source:?}"
        );

        let mut emptied = projection(Vec::new());
        emptied.revision = 3;
        state.set_snapshot(Box::new(emptied));
        assert_eq!(client_run_commands(&state).len(), 1, "{source:?}");
    }
}

#[test]
fn client_run_command_survives_switching_the_active_machine() {
    for source in SOURCES {
        let mut state = state_for(
            &client_config("open-editor"),
            source,
            vec![server_command("cmd_a", &["prefix+s"])],
        );
        let profile = ssh_profile();
        let remote = ClientEndpointId::Ssh(profile.id.clone());
        state.set_endpoint_catalog(&[profile]);
        state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
        let mut remote_projection = projection(vec![server_command("cmd_remote", &["prefix+s"])]);
        remote_projection.boot_id = "remote-boot".into();
        state.set_endpoint_snapshot(&remote, Box::new(remote_projection));
        assert!(state.activate_endpoint_projection(&remote));

        assert_eq!(client_run_commands(&state).len(), 1, "{source:?}");

        assert!(state.activate_endpoint_projection(&ClientEndpointId::Local));
        assert_eq!(client_run_commands(&state).len(), 1, "{source:?}");
    }
}

#[test]
fn client_run_command_survives_a_config_reload() {
    let _guard = crate::config::test_config_env_lock().lock().unwrap();
    let path = std::env::temp_dir().join(format!(
        "herdr-client-run-reload-{}.toml",
        std::process::id()
    ));
    std::fs::write(
        &path,
        r#"
[[keys.command]]
key = "prefix+u"
command = "reloaded-editor"
run_on = "client"

[[keys.command]]
key = "prefix+s"
command = "server-only"
"#,
    )
    .unwrap();
    std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);

    for source in SOURCES {
        let mut state = state_for(
            &client_config("open-editor"),
            source,
            vec![server_command("cmd_a", &["prefix+s"])],
        );
        state.reload_client_config();

        assert_eq!(
            client_run_commands(&state),
            vec![("prefix+u".to_owned(), "reloaded-editor".to_owned())],
            "{source:?}"
        );
        let server_run = state
            .config
            .keybinds
            .keybinds
            .custom_commands
            .iter()
            .filter(|command| command.run_on == CommandRunOn::Server)
            .count();
        // RemoteLocal never shows the endpoint's commands.
        let expected = usize::from(source != ClientShellKeybindingSource::RemoteLocal);
        assert_eq!(server_run, expected, "{source:?}");
    }

    std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
    let _ = std::fs::remove_file(path);
}

#[test]
fn client_run_command_wins_a_key_clash_with_a_server_command() {
    for source in [
        ClientShellKeybindingSource::Local,
        ClientShellKeybindingSource::Endpoint,
    ] {
        let mut state = state_for(
            &client_config("open-editor"),
            source,
            vec![
                server_command("cmd_clash", &["prefix+u", "prefix+y"]),
                server_command("cmd_only_clash", &["prefix+u"]),
            ],
        );

        let client = press_prefixed(&mut state, 'u');
        assert_eq!(run_of(&client).command, "open-editor", "{source:?}");

        let server = press_prefixed(&mut state, 'y');
        let [ClientShellAction::Endpoint { request, .. }] = &server.actions[..] else {
            panic!("expected the server command: {:?}", server.actions);
        };
        let crate::api::schema::Method::CommandInvoke(params) = &request.method else {
            panic!("expected command.invoke");
        };
        assert_eq!(params.command_id, "cmd_clash", "{source:?}");
        // A command left with no key is not bound at all.
        assert!(
            state
                .config
                .keybinds
                .keybinds
                .custom_commands
                .iter()
                .all(|command| command.command != "cmd_only_clash"),
            "{source:?}"
        );
    }
}

#[test]
fn client_run_key_spawns_locally_with_the_focused_pane_and_sends_no_invoke() {
    let cwd = std::env::temp_dir();
    for source in SOURCES {
        let mut state = state_for(&client_config("open-editor"), source, Vec::new());
        let mut local = projection(Vec::new());
        local.panes[0].cwd = Some(cwd.display().to_string());
        local.revision = 2;
        state.set_snapshot(Box::new(local));

        let outcome = press_prefixed(&mut state, 'u');
        let run = run_of(&outcome);

        assert_eq!(run.command, "open-editor");
        assert_eq!(
            run.env,
            vec![
                ("HERDR_ACTIVE_WORKSPACE_ID".to_owned(), "ws_1".to_owned()),
                ("HERDR_ACTIVE_TAB_ID".to_owned(), "tab_1".to_owned()),
                ("HERDR_ACTIVE_PANE_ID".to_owned(), "pane_1".to_owned()),
                (
                    "HERDR_ACTIVE_PANE_CWD".to_owned(),
                    cwd.display().to_string()
                ),
                ("HERDR_ACTIVE_MACHINE".to_owned(), "local".to_owned()),
            ],
            "{source:?}"
        );
        assert_eq!(run.cwd.as_deref(), Some(cwd.as_path()), "{source:?}");
        assert!(state.pending_requests.is_empty(), "{source:?}");
    }
}

#[test]
fn client_run_command_follows_the_pane_the_client_shows() {
    let mut state = state_for(
        &client_config("open-editor"),
        ClientShellKeybindingSource::Local,
        Vec::new(),
    );
    let mut two_panes = projection(Vec::new());
    let mut second = two_panes.panes[0].clone();
    second.pane_id = "pane_2".into();
    second.cwd = Some("/other".into());
    second.focused = false;
    two_panes.panes.push(second);
    two_panes.revision = 2;
    state.set_snapshot(Box::new(two_panes));
    state.predicted_pane_focus = Some(PredictedPaneFocus {
        pane_id: "pane_2".into(),
        request_id: "req".into(),
    });

    let outcome = press_prefixed(&mut state, 'u');
    let run = run_of(&outcome);

    assert_eq!(env_value(run, "HERDR_ACTIVE_PANE_ID"), Some("pane_2"));
    assert_eq!(env_value(run, "HERDR_ACTIVE_PANE_CWD"), Some("/other"));
}

#[test]
fn client_run_command_names_an_ssh_machine_by_its_target_and_ignores_its_paths() {
    let mut state = state_for(
        &client_config("open-editor"),
        ClientShellKeybindingSource::RemoteLocal,
        Vec::new(),
    );
    let profile = ssh_profile();
    let remote = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
    let mut remote_projection = projection(Vec::new());
    remote_projection.boot_id = "remote-boot".into();
    // A directory that exists here, on another machine's behalf.
    remote_projection.panes[0].cwd = Some(std::env::temp_dir().display().to_string());
    state.set_endpoint_snapshot(&remote, Box::new(remote_projection));
    assert!(state.activate_endpoint_projection(&remote));

    let outcome = press_prefixed(&mut state, 'u');
    let run = run_of(&outcome);

    assert_eq!(
        env_value(run, "HERDR_ACTIVE_MACHINE"),
        Some("dev@build.example")
    );
    assert_eq!(
        env_value(run, "HERDR_ACTIVE_PANE_CWD"),
        Some(std::env::temp_dir().display().to_string().as_str())
    );
    assert_eq!(
        run.cwd,
        std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(std::path::PathBuf::from)
    );
}

#[test]
fn client_run_command_leaves_values_unset_without_a_projection() {
    let mut state = ClientShellState::new(
        ClientShellConfig::from_config(&client_config("open-editor"))
            .with_keybinding_source(ClientShellKeybindingSource::RemoteLocal),
    );

    let outcome = press_prefixed(&mut state, 'u');
    let run = run_of(&outcome);

    assert_eq!(
        run.env,
        vec![("HERDR_ACTIVE_MACHINE".to_owned(), "local".to_owned())]
    );
}
