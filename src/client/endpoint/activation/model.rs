use std::time::Instant;

use super::super::ClientEndpointId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EndpointLease {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) generation: u64,
    pub(super) boot_id: String,
    /// The endpoint cache may already be newer than the first activation event. Never let an
    /// activation prove coherence with a revision that the monotonic cache has discarded.
    pub(super) minimum_revision: u64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ActivationEvidence {
    pub(super) snapshot_revision: Option<u64>,
    pub(super) focused_workspace_id: Option<String>,
    pub(super) focused_tab_id: Option<String>,
    pub(super) focused_pane_id: Option<String>,
    pub(super) surface: Option<crate::protocol::PaneSurfaceFrame>,
    /// Presentation effects from the endpoint being activated, in arrival order. They describe
    /// that endpoint, so they wait for its frame instead of reaching the frozen one.
    pub(super) effects: Vec<crate::protocol::ServerMessage>,
}

impl ActivationEvidence {
    pub(super) fn record_effect(&mut self, effect: crate::protocol::ServerMessage) {
        use crate::protocol::ServerMessage;

        match (&effect, self.effects.last_mut()) {
            // Modes and the title are state, so only the latest of each matters.
            (
                ServerMessage::MouseCapture { .. }
                | ServerMessage::ClientShellKeyboardReportAll { .. }
                | ServerMessage::WindowTitle { .. },
                _,
            ) => {
                let kind = std::mem::discriminant(&effect);
                self.effects
                    .retain(|current| std::mem::discriminant(current) != kind);
            }
            (
                ServerMessage::TerminalBell { count },
                Some(ServerMessage::TerminalBell { count: pending }),
            ) => {
                *pending = pending.saturating_add(*count);
                return;
            }
            _ => {}
        }
        self.effects.push(effect);
    }

    pub(super) fn record_snapshot(&mut self, snapshot: &crate::protocol::ClientShellSnapshot) {
        if self
            .snapshot_revision
            .is_none_or(|current| snapshot.revision >= current)
        {
            self.snapshot_revision = Some(snapshot.revision);
            self.focused_workspace_id = snapshot.focused_workspace_id.clone();
            self.focused_tab_id = snapshot.focused_tab_id.clone();
            self.focused_pane_id = snapshot.focused_pane_id.clone();
        }
    }

    pub(super) fn record_surface(&mut self, surface: crate::protocol::PaneSurfaceFrame) {
        let replace = self.surface.as_ref().is_none_or(|current| {
            surface.projection_revision > current.projection_revision
                || (surface.projection_revision == current.projection_revision
                    && surface.surface_revision >= current.surface_revision)
        });
        if replace {
            self.surface = Some(surface);
        }
    }

    /// The endpoint commits every sent patch as its new baseline, so the collected surface must
    /// follow them or the committed frame would start behind the endpoint. Returns whether the
    /// patch continued the collected surface.
    pub(super) fn record_patch(&mut self, patch: &crate::protocol::PaneSurfacePatch) -> bool {
        let Some(surface) = self.surface.as_mut() else {
            return false;
        };
        if surface.boot_id != patch.boot_id
            || surface.projection_revision != patch.projection_revision
            || surface.surface_revision != patch.base_surface_revision
        {
            return false;
        }
        if !crate::client::shell::apply_patch_to_surface(surface, patch) {
            // The endpoint validated this patch against the same baseline, so a failure means the
            // collected surface is corrupt. Never commit a partially patched frame.
            tracing::warn!("activation surface could not follow an endpoint patch");
            self.surface = None;
        }
        true
    }

    pub(super) fn invalidate_surface(&mut self) {
        self.surface = None;
    }

    pub(super) fn coherent_surface(
        &self,
        minimum_revision: u64,
        geometry: crate::protocol::ClientSurfaceSize,
    ) -> Option<&crate::protocol::PaneSurfaceFrame> {
        self.surface.as_ref().filter(|surface| {
            self.snapshot_revision == Some(surface.projection_revision)
                && surface.projection_revision >= minimum_revision
                && super::surface_matches_geometry(surface, geometry)
        })
    }
}

#[derive(Clone, Debug)]
pub(super) enum ActivationPhase {
    ActivatingTarget {
        request_id: String,
        acknowledged_revision: Option<u64>,
        /// At most one focus request may be in flight. Retargets only replace `focus` until
        /// this response arrives, at which point the latest target is sent.
        focus_request_id: Option<String>,
        focus_request_target: Option<crate::client::shell::ClientEndpointFocusTarget>,
        focus_acknowledged: bool,
        evidence: ActivationEvidence,
    },
    ReleasingTargetForRollback {
        request_id: String,
    },
    RestoringSource {
        request_id: String,
        acknowledged_revision: Option<u64>,
        evidence: ActivationEvidence,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SurfaceActivationProgress {
    Pending,
    Ready,
    Rejected(String),
    Stale,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ActivationRollback {
    /// A correlated rollback request is in flight; retain the frozen frame.
    Pending,
    /// Neither endpoint can be made safe to present. Keep pane input frozen while presenting
    /// client chrome and this error.
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ActivationCompletion {
    Activated,
    RestoredSource {
        error: String,
        /// A newer endpoint-qualified selection arrived while this handoff was frozen. It is
        /// started only after the original source has been coherently restored.
        successor: Option<EndpointActivationIntent>,
    },
}

/// What the runtime still owes the endpoint whose frame was just committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommittedActivation {
    pub(crate) completion: ActivationCompletion,
    pub(crate) endpoint_id: ClientEndpointId,
    pub(crate) generation: u64,
    /// Presentation effects the endpoint sent while the frame was frozen, in arrival order.
    pub(crate) effects: Vec<crate::protocol::ServerMessage>,
    /// Keyboard input typed during the switch, in order. Empty unless the target committed.
    pub(crate) input: Vec<crate::protocol::ClientPaneInputEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EndpointActivationIntent {
    pub(crate) endpoint_id: ClientEndpointId,
    pub(crate) target: Option<crate::client::shell::ClientEndpointFocusTarget>,
}

/// Begin failures are separated by whether the transport may already have observed a lifecycle
/// write. A partial transaction must be kept and rolled back under a frozen presentation.
#[derive(Debug)]
pub(crate) enum ActivationBeginError {
    Preflight(String),
    Partial {
        activation: Box<PendingEndpointActivation>,
        error: String,
    },
}

const MAX_BUFFERED_INPUT_EVENTS: usize = 1024;
const MAX_BUFFERED_INPUT_TEXT_BYTES: usize = 1 << 20;

/// Keyboard and paste input typed while the frame is frozen. Mouse events are dropped: their
/// coordinates point into the frozen frame, not the one that will replace it.
#[derive(Debug, Default)]
pub(super) struct BufferedInput {
    events: Vec<crate::protocol::ClientPaneInputEvent>,
    text_bytes: usize,
    overflowed: bool,
}

impl BufferedInput {
    pub(super) fn push(&mut self, events: Vec<crate::protocol::ClientPaneInputEvent>) {
        use crate::protocol::ClientPaneInputEvent;

        for event in events {
            if self.overflowed {
                return;
            }
            let text_bytes = match &event {
                ClientPaneInputEvent::Mouse { .. } => continue,
                ClientPaneInputEvent::Paste(text) | ClientPaneInputEvent::TextCommit(text) => {
                    text.len()
                }
                ClientPaneInputEvent::Key { generated_text, .. } => {
                    generated_text.as_ref().map_or(0, String::len)
                }
            };
            if self.events.len() == MAX_BUFFERED_INPUT_EVENTS
                || self.text_bytes + text_bytes > MAX_BUFFERED_INPUT_TEXT_BYTES
            {
                // A prefix of a typed command can be a different command, so deliver all of the
                // switch's input or none of it.
                *self = Self {
                    overflowed: true,
                    ..Self::default()
                };
                return;
            }
            self.text_bytes += text_bytes;
            self.events.push(event);
        }
    }

    pub(super) fn take(&mut self) -> Vec<crate::protocol::ClientPaneInputEvent> {
        self.text_bytes = 0;
        std::mem::take(&mut self.events)
    }
}

/// The only owner of an endpoint handoff. The registry's active endpoint remains the committed
/// endpoint until the target commits, while this object owns the uncommitted lifecycle lane.
#[derive(Debug)]
pub(crate) struct PendingEndpointActivation {
    pub(super) source: EndpointLease,
    pub(super) source_available: bool,
    pub(super) target: EndpointLease,
    pub(super) focus: Option<crate::client::shell::ClientEndpointFocusTarget>,
    pub(super) host_focused: bool,
    pub(super) resize: crate::protocol::ClientMessage,
    pub(super) phase: ActivationPhase,
    pub(super) deadline: Instant,
    pub(super) epoch: u64,
    pub(super) next_focus_serial: u64,
    pub(super) rollback_error: Option<String>,
    /// A different endpoint was selected while this transaction was in flight. Keep only the
    /// latest intent until source restoration commits.
    pub(super) successor: Option<EndpointActivationIntent>,
    pub(super) input: BufferedInput,
}
