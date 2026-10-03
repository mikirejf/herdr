//! Project grouping for the expanded sidebar: spaces from every machine, grouped by the git
//! repository they check out. A space on a remote machine ends with a dot in that machine's
//! color.

use super::render::{put_text, ShellRenderState};
use super::*;
use crate::config::{SpaceSidebarToken, SpacesSidebarConfig};
use ratatui::{
    text::Line,
    widgets::{Paragraph, Widget},
};

/// Marks a space that runs on a remote machine, in the color of that machine's focused pane
/// border.
const MACHINE_DOT: &str = "●";

/// Blank rows between one project and the next, and before the spaces outside a repository.
const PROJECT_GAP: u16 = 1;

/// Columns from the row start to the status icon of a space outside a repository, and of a
/// project member. A space outside a repository lines up with the header text, and a member
/// sits two columns past it.
const LOOSE_INDENT: u16 = 2;
const MEMBER_INDENT: u16 = 4;

pub(super) enum ProjectRow {
    /// A machine that is not connected. Its row keeps its status and actions reachable.
    Endpoint(usize),
    /// A repository with at least one space across all machines.
    Header {
        key: String,
        label: String,
        collapsed: bool,
        status: crate::api::schema::AgentStatus,
    },
    Workspace {
        endpoint: usize,
        entry: WorkspaceEntry,
    },
}

fn is_linked(workspace: &ClientShellWorkspace) -> bool {
    workspace
        .worktree
        .as_ref()
        .is_some_and(|worktree| worktree.is_linked_worktree)
}

/// Every space takes one line: its status icon and its name, whatever `spaces.rows` says.
fn row_config() -> SpacesSidebarConfig {
    SpacesSidebarConfig {
        rows: vec![vec![
            SpaceSidebarToken::StateIcon,
            SpaceSidebarToken::Workspace,
        ]],
        row_gap: 0,
    }
}

/// Indented rows show their branch, except a linked worktree the user renamed. The header
/// carries the main checkout's label, so its row shows the branch even when renamed.
fn row_tokens(
    workspace: &ClientShellWorkspace,
    entry: &WorkspaceEntry,
    row_config: &SpacesSidebarConfig,
) -> Vec<crate::ui::ResolvedToken> {
    super::sidebar::workspace_rows_labelled(
        workspace,
        workspace.agent_status,
        entry.indented,
        entry.indented && (!workspace.custom_label || !is_linked(workspace)),
        row_config,
    )
    .into_iter()
    .next()
    .unwrap_or_default()
}

/// Rows in visual order. Equal worktree keys on different machines are one project because
/// checkouts move between machines at the same absolute path.
pub(super) fn project_rows(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    collapsed_projects: &HashSet<String>,
) -> Vec<ProjectRow> {
    let mut rows = endpoints
        .iter()
        .enumerate()
        .filter(|(_, endpoint)| endpoint.status != ClientEndpointStatus::Online)
        .map(|(index, _)| ProjectRow::Endpoint(index))
        .collect::<Vec<_>>();

    let mut projects = Vec::<(&crate::protocol::ClientShellWorktree, Vec<(usize, usize)>)>::new();
    let mut project_slots = HashMap::<&str, usize>::new();
    let mut unversioned = Vec::new();
    for (endpoint_index, endpoint) in endpoints.iter().enumerate() {
        let Some(snapshot) = endpoint.snapshot.as_deref() else {
            continue;
        };
        for (workspace_index, workspace) in snapshot.workspaces.iter().enumerate() {
            let Some(worktree) = workspace.worktree.as_ref() else {
                unversioned.push((endpoint_index, workspace_index));
                continue;
            };
            let slot = *project_slots
                .entry(worktree.key.as_str())
                .or_insert_with(|| {
                    projects.push((worktree, Vec::new()));
                    projects.len() - 1
                });
            projects[slot].1.push((endpoint_index, workspace_index));
        }
    }

    let workspace = |(endpoint, index): (usize, usize)| {
        &endpoints[endpoint]
            .snapshot
            .as_deref()
            .expect("project members come from cached snapshots")
            .workspaces[index]
    };
    let single = |(endpoint, index): (usize, usize)| ProjectRow::Workspace {
        endpoint,
        entry: WorkspaceEntry {
            index,
            indented: false,
            last_child: false,
        },
    };
    for (worktree, mut members) in projects {
        // Stable, so ties keep endpoint order and then each endpoint's list order.
        members.sort_by_key(|member| is_linked(workspace(*member)));
        let collapsed = collapsed_projects.contains(&worktree.key);
        // Main checkouts sort first, so a main member leads the list when the project has one.
        let main = workspace(members[0]);
        let label = if is_linked(main) {
            worktree.label.clone()
        } else {
            main.label.clone()
        };
        rows.push(ProjectRow::Header {
            key: worktree.key.clone(),
            label,
            collapsed,
            status: members
                .iter()
                .map(|member| workspace(*member).agent_status)
                .max_by_key(|status| status_priority(*status))
                .unwrap_or(crate::api::schema::AgentStatus::Unknown),
        });
        if collapsed {
            members.retain(|member| {
                &endpoints[member.0].endpoint_id == active_endpoint_id && workspace(*member).focused
            });
        }
        let count = members.len();
        rows.extend(
            members
                .into_iter()
                .enumerate()
                .map(|(position, (endpoint, index))| ProjectRow::Workspace {
                    endpoint,
                    entry: WorkspaceEntry {
                        index,
                        indented: true,
                        last_child: position + 1 == count,
                    },
                }),
        );
    }
    rows.extend(unversioned.into_iter().map(single));
    rows
}

/// The header label: the user's name for the project, else the label `project_rows` derived.
pub(super) fn header_label<'a>(
    project_names: &'a BTreeMap<String, String>,
    key: &str,
    derived: &'a str,
) -> &'a str {
    project_names.get(key).map_or(derived, String::as_str)
}

/// The main checkout root of a project whose worktree key is its `.git` directory. A key of
/// another shape, such as a bare repository, has no checkout to start a worktree from.
pub(super) fn checkout_root(key: &str) -> Option<&str> {
    // An endpoint path, so either separator can occur whatever the client's OS.
    key.strip_suffix(".git")?
        .strip_suffix(['/', '\\'])
        .filter(|root| !root.is_empty())
}

/// Whether menus offer `endpoint` as a machine to create a space or worktree on.
pub(super) fn machine_is_offered(endpoint: &ClientShellEndpoint) -> bool {
    endpoint.status == ClientEndpointStatus::Online && endpoint.snapshot.is_some()
}

/// Machines menus offer, in sidebar order: Local first, then the saved machines.
pub(super) fn offered_machines(endpoints: &[ClientShellEndpoint]) -> Vec<ClientMenuMachine> {
    endpoints
        .iter()
        .filter(|endpoint| machine_is_offered(endpoint))
        .map(|endpoint| (endpoint.endpoint_id.clone(), endpoint.label.clone()))
        .collect()
}

/// Workspaces in project order across machines, as `(endpoint index, workspace index)`.
pub(super) fn project_workspace_order(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    collapsed_projects: &HashSet<String>,
) -> Vec<(usize, usize)> {
    project_rows(endpoints, active_endpoint_id, collapsed_projects)
        .into_iter()
        .filter_map(|row| match row {
            ProjectRow::Workspace { endpoint, entry } => Some((endpoint, entry.index)),
            _ => None,
        })
        .collect()
}

fn workspace_of<'a>(
    endpoints: &'a [ClientShellEndpoint],
    endpoint: usize,
    entry: &WorkspaceEntry,
) -> Option<&'a ClientShellWorkspace> {
    endpoints[endpoint]
        .snapshot
        .as_deref()
        .and_then(|snapshot| snapshot.workspaces.get(entry.index))
}

/// Draws the `projects` section title and list into `workspace_area`, leaving the footer row.
pub(super) fn render_list(
    buffer: &mut Buffer,
    workspace_area: Rect,
    active_snapshot: Option<&ClientShellSnapshot>,
    config: &ClientShellConfig,
    state: &mut ShellRenderState<'_>,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    put_text(
        buffer,
        workspace_area.x,
        workspace_area.y,
        workspace_area.width,
        " projects",
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    let endpoints = state.endpoints;
    let rows = project_rows(
        endpoints,
        state.active_endpoint_id,
        state.collapsed_projects,
    );
    let body = Rect::new(
        workspace_area.x,
        workspace_area.y.saturating_add(WORKSPACE_HEADER_ROWS),
        workspace_area.width,
        workspace_area
            .height
            .saturating_sub(WORKSPACE_HEADER_ROWS + 1),
    );
    hits.workspace_body = body;
    let row_heights = vec![1; rows.len()];
    let gaps = rows
        .iter()
        .enumerate()
        .map(|(index, row)| match (row, rows.get(index + 1)) {
            (ProjectRow::Endpoint(_), _) | (_, None | Some(ProjectRow::Endpoint(_))) => 0,
            (_, Some(ProjectRow::Header { .. })) => PROJECT_GAP,
            (
                ProjectRow::Workspace { entry, .. },
                Some(ProjectRow::Workspace { entry: next, .. }),
            ) if entry.indented && !next.indented => PROJECT_GAP,
            _ => 0,
        })
        .collect::<Vec<_>>();

    let reveal_navigation = !body.is_empty() && std::mem::take(state.reveal_navigation_workspace);
    let reveal_focus = !body.is_empty() && std::mem::take(state.reveal_focused_workspace);
    if reveal_navigation || reveal_focus {
        let target = rows.iter().position(|row| {
            let ProjectRow::Workspace { endpoint, entry } = row else {
                return false;
            };
            let endpoint_id = &endpoints[*endpoint].endpoint_id;
            workspace_of(endpoints, *endpoint, entry).is_some_and(|workspace| {
                if reveal_navigation {
                    state
                        .selected_workspace_id
                        .is_some_and(|target| target.matches(endpoint_id, &workspace.workspace_id))
                } else {
                    endpoint_id == state.active_endpoint_id
                        && active_snapshot.is_some_and(|snapshot| {
                            snapshot.focused_workspace_id.as_deref()
                                == Some(workspace.workspace_id.as_str())
                        })
                }
            })
        });
        if let Some(target) = target {
            *state.workspace_scroll = super::scroll::list_scroll_start_to_reveal(
                &row_heights,
                &gaps,
                body.height,
                *state.workspace_scroll,
                target,
            );
        }
    }
    let metrics = super::scroll::list_scroll_metrics(
        &row_heights,
        &gaps,
        body.height,
        *state.workspace_scroll,
    );
    hits.workspace_max_scroll = metrics.max_offset_from_bottom;
    hits.workspace_scroll_metrics = Some(metrics);
    *state.workspace_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let show_scrollbar = metrics.max_offset_from_bottom > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));

    let mut y = body.y;
    for (row_index, row) in rows.iter().enumerate().skip(*state.workspace_scroll) {
        let height = row_heights[row_index].min(body.height);
        if y.saturating_add(height) > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, content_width, height);
        match row {
            ProjectRow::Endpoint(index) => {
                let endpoint = &endpoints[*index];
                let status_badge = super::endpoint_sidebar::render_endpoint_row(
                    buffer,
                    rect,
                    "",
                    endpoint,
                    false,
                    state.machine_diagnostics,
                    palette,
                );
                // Machine collapse has no meaning here, so the row offers no collapse toggle.
                hits.machines.push(MachineHit {
                    rect,
                    status_badge,
                    collapse_toggle: Rect::default(),
                    endpoint_id: endpoint.endpoint_id.clone(),
                });
            }
            ProjectRow::Header {
                key,
                label,
                collapsed,
                status,
            } => {
                render_header(
                    buffer,
                    rect,
                    header_label(state.project_names, key, label),
                    collapsed.then_some(*status),
                    config,
                );
                if *collapsed {
                    put_text(
                        buffer,
                        rect.right().saturating_sub(1),
                        rect.y,
                        u16::from(rect.width > 0),
                        "▸",
                        Style::default().fg(palette.accent),
                    );
                }
                hits.projects.push((rect, key.clone()));
            }
            ProjectRow::Workspace {
                endpoint: endpoint_index,
                entry,
            } => {
                let endpoint = &endpoints[*endpoint_index];
                let Some(workspace) = workspace_of(endpoints, *endpoint_index, entry) else {
                    continue;
                };
                render_workspace(buffer, rect, endpoint, workspace, entry, config, state);
                hits.workspaces.push(WorkspaceHit {
                    rect,
                    endpoint_id: endpoint.endpoint_id.clone(),
                    workspace_id: workspace.workspace_id.clone(),
                    indented: entry.indented,
                    group_toggle: None,
                });
            }
        }
        y = y.saturating_add(height).saturating_add(gaps[row_index]);
    }
    if show_scrollbar {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.workspace_scrollbar = track;
        super::scroll::render_list_scrollbar(buffer, track, metrics, palette);
    }
}

/// The repository name, aligned with the names of unindented rows. A collapsed project
/// shows the most urgent status of its spaces, because their own rows are hidden.
fn render_header(
    buffer: &mut Buffer,
    rect: Rect,
    label: &str,
    collapsed_status: Option<crate::api::schema::AgentStatus>,
    config: &ClientShellConfig,
) {
    let palette = &config.palette;
    let mut x = rect.x.saturating_add(2);
    let right = rect.right().saturating_sub(2);
    if let Some(status) = collapsed_status {
        x = super::render::put_segment(
            buffer,
            x,
            rect.y,
            right,
            status_icon(status, config.status_indicators),
            Style::default().fg(status_color(status, palette)),
        );
        x = super::render::put_segment(buffer, x, rect.y, right, " ", Style::default());
    }
    put_text(
        buffer,
        x,
        rect.y,
        right.saturating_sub(x),
        label,
        Style::default()
            .fg(palette.subtext0)
            .add_modifier(Modifier::BOLD),
    );
}

fn render_workspace(
    buffer: &mut Buffer,
    rect: Rect,
    endpoint: &ClientShellEndpoint,
    workspace: &ClientShellWorkspace,
    entry: &WorkspaceEntry,
    config: &ClientShellConfig,
    state: &ShellRenderState<'_>,
) {
    let palette = &config.palette;
    let status = workspace.agent_status;
    let focused = &endpoint.endpoint_id == state.active_endpoint_id && workspace.focused;
    let selected = state
        .selected_workspace_id
        .is_some_and(|target| target.matches(&endpoint.endpoint_id, &workspace.workspace_id));

    let x = rect.x.saturating_add(if entry.indented {
        MEMBER_INDENT
    } else {
        LOOSE_INDENT
    });
    // Two columns stay free at the right: a space, then the machine dot.
    let right = rect.right().saturating_sub(2);
    let tokens = row_tokens(workspace, entry, &row_config());
    let spans = crate::ui::resolved_token_spans(
        &tokens,
        (
            status_icon(status, config.status_indicators),
            Style::default().fg(status_color(status, palette)),
        ),
        Style::default().fg(status_color(status, palette)),
        Style::default()
            .fg(if focused {
                palette.text
            } else {
                palette.subtext0
            })
            .add_modifier(if focused {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        Style::default().fg(if focused {
            palette.mauve
        } else {
            palette.overlay0
        }),
        Style::default().fg(palette.overlay1),
        palette,
        right.saturating_sub(x) as usize,
    );
    Paragraph::new(Line::from(spans))
        .render(Rect::new(x, rect.y, right.saturating_sub(x), 1), buffer);

    if endpoint.status != ClientEndpointStatus::Online {
        buffer.set_style(
            rect,
            Style::default()
                .fg(palette.overlay0)
                .add_modifier(Modifier::DIM),
        );
    }
    let background = if selected {
        Some(super::sidebar::workspace_selection_background(palette))
    } else if focused {
        Some(super::sidebar::workspace_active_background(
            palette,
            state.selected_workspace_id.is_some(),
        ))
    } else {
        None
    };
    if let Some(background) = background {
        buffer.set_style(rect, Style::default().bg(background));
    }
    if endpoint.endpoint_id != ClientEndpointId::Local {
        put_text(
            buffer,
            rect.right().saturating_sub(1),
            rect.y,
            u16::from(rect.width > 0),
            MACHINE_DOT,
            Style::default().fg(machine_color(endpoint, config)),
        );
    }
}

/// The color of the machine's focused pane border: its configured machine color, else the
/// machine's own focus accent.
pub(super) fn machine_color(
    endpoint: &ClientShellEndpoint,
    config: &ClientShellConfig,
) -> ratatui::style::Color {
    config
        .machine_focus_colors
        .get(&endpoint.label)
        .copied()
        .or_else(|| {
            endpoint
                .snapshot
                .as_deref()?
                .pane_focus_style
                .map(|style| style.colors().accent)
        })
        .unwrap_or(config.palette.accent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ClientShellWorktree;

    const DEVKIT: &str = "/Users/andrej/dev-work/arx1/devkit/.git";
    const DOTFILES: &str = "/Users/andrej/dotfiles/.git";

    fn workspace(id: &str, project: Option<(&str, &str, bool)>) -> ClientShellWorkspace {
        ClientShellWorkspace {
            workspace_id: id.into(),
            label: id.into(),
            focused: false,
            worktree: project.map(|(key, label, linked)| ClientShellWorktree {
                key: key.into(),
                label: label.into(),
                is_linked_worktree: linked,
            }),
            ..super::super::tests::snapshot().workspaces[0].clone()
        }
    }

    fn endpoint(label: &str, workspaces: Vec<ClientShellWorkspace>) -> ClientShellEndpoint {
        let mut endpoint = local_endpoint();
        if label != "Local" {
            endpoint.endpoint_id = ClientEndpointId::Ssh(
                crate::client::endpoint::ProfileId::parse(format!("{:0>32}", label.len())).unwrap(),
            );
            endpoint.label = label.into();
        }
        let mut snapshot = super::super::tests::snapshot();
        snapshot.workspaces = workspaces;
        endpoint.snapshot = Some(Box::new(snapshot));
        endpoint
    }

    /// One line per row: `machine:id` for spaces, indented for project members.
    fn outline(endpoints: &[ClientShellEndpoint], collapsed: &[&str]) -> Vec<String> {
        let collapsed = collapsed.iter().map(|key| key.to_string()).collect();
        project_rows(endpoints, &ClientEndpointId::Local, &collapsed)
            .into_iter()
            .map(|row| match row {
                ProjectRow::Endpoint(index) => format!("machine {}", endpoints[index].label),
                ProjectRow::Header {
                    label, collapsed, ..
                } => format!("project {label}{}", if collapsed { " ▸" } else { "" }),
                ProjectRow::Workspace { endpoint, entry } => {
                    let workspace = workspace_of(endpoints, endpoint, &entry).unwrap();
                    format!(
                        "{}{}:{}{}",
                        if entry.indented { "  " } else { "" },
                        endpoints[endpoint].label,
                        workspace.workspace_id,
                        if entry.last_child { " (last)" } else { "" }
                    )
                }
            })
            .collect()
    }

    fn fleet() -> Vec<ClientShellEndpoint> {
        vec![
            endpoint(
                "Local",
                vec![
                    workspace("scratch", None),
                    workspace("test-audit", Some((DEVKIT, "devkit", true))),
                    workspace("dotfiles", Some((DOTFILES, "dotfiles", false))),
                    workspace("devkit", Some((DEVKIT, "devkit", false))),
                ],
            ),
            endpoint(
                "jan-box",
                vec![
                    workspace("mutants", Some((DEVKIT, "devkit", true))),
                    workspace("devkit-jan", Some((DEVKIT, "devkit", false))),
                ],
            ),
        ]
    }

    #[test]
    fn groups_spaces_from_every_machine_by_worktree_key() {
        assert_eq!(
            outline(&fleet(), &[]),
            [
                "project devkit",
                "  Local:devkit",
                "  jan-box:devkit-jan",
                "  Local:test-audit",
                "  jan-box:mutants (last)",
                "project dotfiles",
                "  Local:dotfiles (last)",
                "Local:scratch",
            ]
        );
    }

    #[test]
    fn project_without_a_main_checkout_still_groups() {
        let endpoints = vec![
            endpoint(
                "Local",
                vec![workspace("test-audit", Some((DEVKIT, "devkit", true)))],
            ),
            endpoint(
                "jan-box",
                vec![workspace("mutants", Some((DEVKIT, "devkit", true)))],
            ),
        ];
        assert_eq!(
            outline(&endpoints, &[]),
            [
                "project devkit",
                "  Local:test-audit",
                "  jan-box:mutants (last)"
            ]
        );
    }

    #[test]
    fn collapsed_project_keeps_the_active_machines_focused_space() {
        let mut endpoints = fleet();
        for endpoint in &mut endpoints {
            // Every machine has a focused space; only the active machine's counts.
            let snapshot = endpoint.snapshot.as_mut().unwrap();
            let last = snapshot.workspaces.len() - 1;
            snapshot.workspaces[last].focused = true;
            snapshot.workspaces[last].agent_status = crate::api::schema::AgentStatus::Working;
        }
        endpoints[0].snapshot.as_mut().unwrap().workspaces[1].agent_status =
            crate::api::schema::AgentStatus::Blocked;
        assert_eq!(
            outline(&endpoints, &[DEVKIT]),
            [
                "project devkit ▸",
                "  Local:devkit (last)",
                "project dotfiles",
                "  Local:dotfiles (last)",
                "Local:scratch",
            ]
        );
        let rows = project_rows(
            &endpoints,
            &ClientEndpointId::Local,
            &HashSet::from([DEVKIT.to_owned()]),
        );
        assert!(matches!(
            rows[0],
            ProjectRow::Header {
                status: crate::api::schema::AgentStatus::Blocked,
                ..
            }
        ));
    }

    #[test]
    fn only_machines_that_are_not_connected_get_rows() {
        let mut endpoints = fleet();
        endpoints[1].status = ClientEndpointStatus::Reconnecting;
        let rows = outline(&endpoints, &[]);
        assert_eq!(rows[0], "machine jan-box");
        assert!(rows[1..].iter().all(|row| !row.starts_with("machine")));
        // Cached spaces of the disconnected machine stay in their project.
        assert!(rows.contains(&"  jan-box:mutants (last)".to_owned()));
    }

    #[test]
    fn checkout_root_is_the_parent_of_a_git_directory_key() {
        assert_eq!(
            checkout_root(DEVKIT),
            Some("/Users/andrej/dev-work/arx1/devkit")
        );
        assert_eq!(checkout_root(r"C:\src\herdr\.git"), Some(r"C:\src\herdr"));
        assert_eq!(checkout_root("/srv/git/devkit.git"), None);
        assert_eq!(checkout_root("/srv/git/devkit"), None);
        assert_eq!(checkout_root("/.git"), None);
    }

    #[test]
    fn saved_name_wins_over_the_derived_label() {
        let names = BTreeMap::from([(DEVKIT.to_owned(), "Devkit".to_owned())]);
        assert_eq!(header_label(&names, DEVKIT, "devkit-main"), "Devkit");
        assert_eq!(header_label(&names, DOTFILES, "dotfiles"), "dotfiles");
    }

    #[test]
    fn workspace_order_lists_spaces_in_row_order() {
        assert_eq!(
            project_workspace_order(&fleet(), &ClientEndpointId::Local, &HashSet::new()),
            [(0, 3), (1, 1), (0, 1), (1, 0), (0, 2), (0, 0)]
        );
    }
}
