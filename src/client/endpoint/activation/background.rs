use crate::protocol::{ClientMessage, PaneSurfaceFrame, ServerMessage};

use super::super::ClientEndpointId;
use super::model::ActivationEvidence;
use super::protocol::{resize_geometry, surface_matches_geometry};

/// The surface of an endpoint that holds no presentation lease but still streams to this
/// client. The client follows that stream here, so it can present the endpoint again without
/// waiting for it.
#[derive(Debug)]
pub(crate) struct BackgroundSurface {
    boot_id: String,
    /// The last geometry the endpoint received. Its stream renders at this size.
    resize: ClientMessage,
    /// Only the surface and the host modes are kept. Snapshots stay in the shell's endpoint
    /// cache, which a commit checks the surface against.
    evidence: ActivationEvidence,
}

/// What following a background stream did with one message.
#[derive(Debug)]
pub(crate) enum BackgroundDelivery {
    /// The message belongs to the background surface and needs nothing more.
    Kept,
    /// A patch did not continue the kept surface. The endpoint must send a complete one.
    Diverged,
    /// Not a presentation message; route it as usual.
    Other(Box<ServerMessage>),
}

impl BackgroundSurface {
    pub(super) fn new(
        boot_id: String,
        resize: ClientMessage,
        surface: Option<PaneSurfaceFrame>,
    ) -> Self {
        let mut evidence = ActivationEvidence::default();
        if let Some(surface) = surface.filter(|surface| surface.boot_id == boot_id) {
            evidence.record_surface(surface);
        }
        Self {
            boot_id,
            resize,
            evidence,
        }
    }

    pub(crate) fn follow(&mut self, message: Box<ServerMessage>) -> BackgroundDelivery {
        match *message {
            ServerMessage::PaneSurface(surface) => {
                if surface.boot_id == self.boot_id {
                    self.evidence.record_surface(surface);
                }
                BackgroundDelivery::Kept
            }
            ServerMessage::PaneSurfacePatch(patch) => {
                if patch.boot_id != self.boot_id || self.evidence.record_patch(&patch) {
                    return BackgroundDelivery::Kept;
                }
                // A patch from an older projection than the kept surface is late, not a gap.
                if self
                    .evidence
                    .surface
                    .as_ref()
                    .is_some_and(|surface| patch.projection_revision < surface.projection_revision)
                {
                    return BackgroundDelivery::Kept;
                }
                self.evidence.invalidate_surface();
                BackgroundDelivery::Diverged
            }
            ServerMessage::MouseCapture { .. }
            | ServerMessage::ClientShellKeyboardReportAll { .. } => {
                self.evidence.record_effect(*message);
                BackgroundDelivery::Kept
            }
            // Nothing is presented from this endpoint, so its title, bells and graphics are moot.
            // A background stream carries graphics inline, never as files that need an answer.
            ServerMessage::WindowTitle { .. }
            | ServerMessage::TerminalBell { .. }
            | ServerMessage::Graphics { .. } => BackgroundDelivery::Kept,
            other => BackgroundDelivery::Other(Box::new(other)),
        }
    }

    /// The kept surface and host modes, when they prove the endpoint's current presentation for
    /// this geometry and navigation target.
    pub(super) fn presentable(
        &self,
        shell: &crate::client::shell::ClientShellState,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        resize: &ClientMessage,
        focus: Option<&crate::client::shell::ClientEndpointFocusTarget>,
    ) -> Option<(&PaneSurfaceFrame, &[ServerMessage])> {
        let surface = self.evidence.surface.as_ref()?;
        let snapshot = shell.endpoint_snapshot(endpoint_id, generation)?;
        let geometry = resize_geometry(resize)?;
        let effects = self.evidence.effects.as_slice();
        let has_modes = effects
            .iter()
            .any(|effect| matches!(effect, ServerMessage::MouseCapture { .. }))
            && effects
                .iter()
                .any(|effect| matches!(effect, ServerMessage::ClientShellKeyboardReportAll { .. }));
        // Native graphics are uploaded only to a presenting connection, so a surface with images
        // needs the endpoint's full activation.
        let presentable = *resize == self.resize
            && has_modes
            && snapshot.boot_id == self.boot_id
            && snapshot.revision == surface.projection_revision
            && surface_matches_geometry(surface, geometry)
            && surface.graphics.placements.is_empty()
            && surface.graphics.retained_assets.is_empty()
            && match focus {
                None => true,
                Some(crate::client::shell::ClientEndpointFocusTarget::Workspace(id)) => {
                    snapshot.focused_workspace_id.as_ref() == Some(id)
                }
                Some(crate::client::shell::ClientEndpointFocusTarget::Tab(id)) => {
                    snapshot.focused_tab_id.as_ref() == Some(id)
                }
                Some(crate::client::shell::ClientEndpointFocusTarget::Pane(id)) => {
                    snapshot.focused_pane_id.as_ref() == Some(id)
                        && surface
                            .panes
                            .iter()
                            .any(|pane| pane.focused && &pane.pane_id == id)
                }
            };
        presentable.then_some((surface, effects))
    }
}
