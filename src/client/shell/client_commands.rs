use super::*;

/// Names the machine the user is looking at: `local`, or the SSH target of a remote machine.
const ACTIVE_MACHINE_ENV_VAR: &str = "HERDR_ACTIVE_MACHINE";

/// A custom command the client starts itself, on the machine that runs the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClientCommandRun {
    pub(crate) command: String,
    /// Added to the client's own environment.
    pub(crate) env: Vec<(String, String)>,
    /// `None` keeps the client's working directory.
    pub(crate) cwd: Option<std::path::PathBuf>,
}

impl ClientShellState {
    /// The command to start for a key bound to a client-run command. Every value comes from what
    /// this client shows, not from the focus of the endpoint, which may differ during a switch.
    pub(super) fn client_command_run(&self, command: &str) -> ClientCommandRun {
        let mut env = Vec::new();
        let mut push = |name: &str, value: Option<String>| {
            if let Some(value) = value {
                env.push((name.to_owned(), value));
            }
        };
        let pane_id = self.focused_pane_id();
        let pane_cwd = pane_id.as_deref().and_then(|pane_id| {
            self.snapshot
                .as_deref()?
                .panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)?
                .cwd
                .clone()
        });
        push(
            "HERDR_ACTIVE_WORKSPACE_ID",
            self.effective_focused_workspace_id().map(str::to_owned),
        );
        push(
            "HERDR_ACTIVE_TAB_ID",
            self.effective_focused_tab_id().map(str::to_owned),
        );
        push("HERDR_ACTIVE_PANE_ID", pane_id);
        push("HERDR_ACTIVE_PANE_CWD", pane_cwd.clone());
        push(ACTIVE_MACHINE_ENV_VAR, self.active_machine_name());

        // A path from another machine means nothing here.
        let cwd = pane_cwd
            .filter(|_| self.active_endpoint_id.is_local())
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_dir())
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|home| !home.is_empty())
                    .map(std::path::PathBuf::from)
            });
        ClientCommandRun {
            command: command.to_owned(),
            env,
            cwd,
        }
    }

    fn active_machine_name(&self) -> Option<String> {
        match &self.active_endpoint_id {
            ClientEndpointId::Local => Some("local".to_owned()),
            endpoint_id @ ClientEndpointId::Ssh(_) => self
                .endpoints
                .iter()
                .find(|endpoint| &endpoint.endpoint_id == endpoint_id)?
                .ssh_target
                .clone(),
        }
    }
}
