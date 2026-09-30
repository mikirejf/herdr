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
    /// this geometry and navigation target. Otherwise, the checks that failed.
    pub(super) fn presentable(
        &self,
        shell: &crate::client::shell::ClientShellState,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        resize: &ClientMessage,
        focus: Option<&crate::client::shell::ClientEndpointFocusTarget>,
    ) -> Result<(&PaneSurfaceFrame, &[ServerMessage]), BackgroundMiss> {
        let mut miss = BackgroundMiss::default();
        let Some(surface) = self.evidence.surface.as_ref() else {
            miss.surface = true;
            return Err(miss);
        };
        let Some(snapshot) = shell.endpoint_snapshot(endpoint_id, generation) else {
            miss.snapshot = true;
            return Err(miss);
        };
        let Some(geometry) = resize_geometry(resize) else {
            miss.resize = true;
            return Err(miss);
        };
        let effects = self.evidence.effects.as_slice();
        miss.resize = *resize != self.resize;
        miss.modes = !(effects
            .iter()
            .any(|effect| matches!(effect, ServerMessage::MouseCapture { .. }))
            && effects.iter().any(|effect| {
                matches!(effect, ServerMessage::ClientShellKeyboardReportAll { .. })
            }));
        miss.boot_id = snapshot.boot_id != self.boot_id;
        miss.revision = snapshot.revision != surface.projection_revision;
        miss.geometry = !surface_matches_geometry(surface, geometry);
        // Native graphics are uploaded only to a presenting connection, so a surface with images
        // needs the endpoint's full activation.
        miss.graphics =
            !surface.graphics.placements.is_empty() || !surface.graphics.retained_assets.is_empty();
        miss.focus = !match focus {
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
        if miss == BackgroundMiss::default() {
            Ok((surface, effects))
        } else {
            Err(miss)
        }
    }
}

/// The checks that kept a background surface from being presented at once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BackgroundMiss {
    surface: bool,
    snapshot: bool,
    resize: bool,
    modes: bool,
    boot_id: bool,
    revision: bool,
    geometry: bool,
    graphics: bool,
    focus: bool,
}

impl std::fmt::Display for BackgroundMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let failed = [
            (self.surface, "surface"),
            (self.snapshot, "snapshot"),
            (self.resize, "resize"),
            (self.modes, "modes"),
            (self.boot_id, "boot_id"),
            (self.revision, "revision"),
            (self.geometry, "geometry"),
            (self.graphics, "graphics"),
            (self.focus, "focus"),
        ]
        .into_iter()
        .filter_map(|(failed, check)| failed.then_some(check));
        for (index, check) in failed.enumerate() {
            if index > 0 {
                f.write_str("+")?;
            }
            f.write_str(check)?;
        }
        Ok(())
    }
}

/// Why a switch ran a full activation instead of presenting a kept background surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WarmPathMiss {
    /// This connection keeps no background surface for the endpoint.
    NotKept,
    /// The kept surface failed these checks.
    Unpresentable(BackgroundMiss),
    /// The endpoint could not be told that it presents again.
    Unsent,
}

impl std::fmt::Display for WarmPathMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotKept => f.write_str("not_kept"),
            Self::Unpresentable(miss) => write!(f, "stale:{miss}"),
            Self::Unsent => f.write_str("unsent"),
        }
    }
}
