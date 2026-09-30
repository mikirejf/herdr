use std::time::{Duration, Instant};

use super::{ClientEndpointId, ClientEndpointStatus, EndpointRegistry, EndpointSendOutcome};

mod background;
mod model;
mod protocol;
pub(crate) use background::{BackgroundDelivery, BackgroundSurface, WarmPathMiss};
pub(crate) use model::{
    ActivationBeginError, ActivationCompletion, ActivationRollback, CommittedActivation,
    EndpointActivationIntent, PendingEndpointActivation, SurfaceActivationProgress,
};
use model::{ActivationEvidence, ActivationPhase, BufferedInput, EndpointLease};
use protocol::*;

const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);
/// A switch that commits later than this is written to the client log, so a freeze noticed in
/// use can be diagnosed afterwards. Healthy switches take one round trip.
const SLOW_ACTIVATION: Duration = Duration::from_secs(1);

fn release_surface_best_effort(
    lease: &EndpointLease,
    endpoints: &mut EndpointRegistry,
    request_id: String,
) {
    if !endpoints.accepts(&lease.endpoint_id, lease.generation) {
        return;
    }
    endpoints.set_surface_active(&lease.endpoint_id, false);
    endpoints.set_background_surface(&lease.endpoint_id, None);
    let _ = endpoints.send_to(
        &lease.endpoint_id,
        &crate::protocol::ClientMessage::ClientShellFocus { focused: false },
    );
    match surface_interest_request(&lease.boot_id, request_id, false) {
        Ok(request) => {
            let _ = endpoints.send_to(&lease.endpoint_id, &request);
        }
        Err(error) => tracing::warn!(%error, "could not request abandoned surface cleanup"),
    }
}

/// Release the presenting source. A remote source keeps streaming its surface in the background,
/// so switching back to it needs no round trip. Local already answers within a millisecond, so
/// it stops rendering for this client instead.
fn release_source(
    lease: &EndpointLease,
    background: Option<BackgroundSurface>,
    endpoints: &mut EndpointRegistry,
    request_id: String,
) {
    let Some(background) = background else {
        release_surface_best_effort(lease, endpoints, request_id);
        return;
    };
    if !endpoints.accepts(&lease.endpoint_id, lease.generation) {
        return;
    }
    endpoints.set_surface_active(&lease.endpoint_id, false);
    endpoints.set_background_surface(&lease.endpoint_id, Some(background));
    let _ = endpoints.send_to(
        &lease.endpoint_id,
        &crate::protocol::ClientMessage::ClientShellFocus { focused: false },
    );
    let _ = endpoints.send_to(
        &lease.endpoint_id,
        &crate::protocol::ClientMessage::EndpointControl {
            kind: crate::protocol::endpoint::SURFACE_BACKGROUND_KIND.into(),
            data: String::new(),
        },
    );
}

/// The presenting source's surface, seeded from what the shell shows, when the source should
/// keep streaming after it is released.
fn source_background(
    shell: &crate::client::shell::ClientShellState,
    source: &EndpointLease,
    target: &ClientEndpointId,
    resize: &crate::protocol::ClientMessage,
) -> Option<BackgroundSurface> {
    (!source.endpoint_id.is_local()
        && source.endpoint_id != *target
        && shell.endpoint_is_active(&source.endpoint_id))
    .then(|| {
        BackgroundSurface::new(
            source.boot_id.clone(),
            resize.clone(),
            shell.followed_pane_surface().cloned(),
        )
    })
}

/// Present a background endpoint's kept surface at once, then tell the endpoint it presents
/// again. Returns why not, with the presentation unchanged, when the kept surface cannot be proven
/// current or the endpoint cannot be reached; the caller then runs a full activation, which also
/// reports a lost connection.
pub(crate) fn present_background_surface(
    shell: &mut crate::client::shell::ClientShellState,
    endpoints: &mut EndpointRegistry,
    target: &ClientEndpointId,
    focus: Option<&crate::client::shell::ClientEndpointFocusTarget>,
    resize: &crate::protocol::ClientMessage,
    serial: u64,
) -> Result<CommittedActivation, WarmPathMiss> {
    let generation = endpoints
        .connection(target)
        .ok_or(WarmPathMiss::NotKept)?
        .generation;
    let (surface, effects) = endpoints
        .background_surface(target)
        .ok_or(WarmPathMiss::NotKept)?
        .presentable(shell, target, generation, resize, focus)
        .map_err(WarmPathMiss::Unpresentable)?;
    let (surface, effects) = (surface.clone(), effects.to_vec());
    let source_id = endpoints.active_id().clone();
    let source = endpoints
        .connection(&source_id)
        .is_some_and(|connection| connection.surface_active)
        .then(|| endpoint_lease(shell, endpoints, &source_id).ok())
        .flatten();

    // The foreground control keeps the endpoint's projection epoch, so the stream that follows
    // continues from the surface presented here. Focus needs the lease, so it comes second.
    for message in [
        crate::protocol::ClientMessage::EndpointControl {
            kind: crate::protocol::endpoint::SURFACE_FOREGROUND_KIND.into(),
            data: String::new(),
        },
        crate::protocol::ClientMessage::ClientShellFocus {
            focused: shell.host_focus_baseline(),
        },
    ] {
        if endpoints.send_to(target, &message) != EndpointSendOutcome::Sent {
            return Err(WarmPathMiss::Unsent);
        }
    }
    if let Some(source) = source {
        let background = source_background(shell, &source, target, resize);
        release_source(
            &source,
            background,
            endpoints,
            format!("client-shell-surface:{serial}:off"),
        );
    }

    endpoints.set_background_surface(target, None);
    endpoints.set_surface_active(target, true);
    shell.set_endpoint_status(target, ClientEndpointStatus::Online);
    // The kept surface was checked against this connection's snapshot, and the sends above kept
    // the connection, so both steps hold.
    assert!(
        endpoints.set_active(target),
        "a sent-to endpoint stays connected"
    );
    assert!(
        shell.activate_endpoint_projection(target),
        "a presentable endpoint has a snapshot"
    );
    shell.set_pane_surface(surface);
    Ok(CommittedActivation {
        completion: ActivationCompletion::Activated,
        endpoint_id: target.clone(),
        generation,
        effects,
        input: Vec::new(),
    })
}

impl PendingEndpointActivation {
    pub(crate) fn prepare(
        shell: &crate::client::shell::ClientShellState,
        endpoints: &EndpointRegistry,
        target: ClientEndpointId,
        focus: Option<crate::client::shell::ClientEndpointFocusTarget>,
        resize: crate::protocol::ClientMessage,
        serial: u64,
        warm_path: WarmPathMiss,
        now: Instant,
    ) -> Result<Self, ActivationBeginError> {
        resize_geometry(&resize).ok_or_else(|| {
            ActivationBeginError::Preflight(
                "endpoint activation did not include a surface resize".to_owned(),
            )
        })?;
        let source_id = endpoints.active_id().clone();
        let source_has_live_surface = endpoints
            .connection(&source_id)
            .is_some_and(|connection| connection.surface_active);
        let (source, source_available) = if source_has_live_surface {
            (
                endpoint_lease(shell, endpoints, &source_id)
                    .map_err(ActivationBeginError::Preflight)?,
                true,
            )
        } else {
            (disconnected_endpoint_lease(shell, &source_id), false)
        };
        let target_lease =
            endpoint_lease(shell, endpoints, &target).map_err(ActivationBeginError::Preflight)?;
        let source_compatible = !source_available
            || endpoints
                .connection(&source_id)
                .is_some_and(|connection| connection.negotiation.supports_surface_interest());
        let target_compatible = endpoints
            .connection(&target)
            .is_some_and(|connection| connection.negotiation.supports_surface_interest());
        if !source_compatible || !target_compatible {
            return Err(ActivationBeginError::Preflight(
                "endpoint must be updated before it can join the selected surface".into(),
            ));
        }

        let source_is_target = source.endpoint_id == target_lease.endpoint_id;
        // Validate every typed lifecycle and optional focus envelope before the first transport
        // write. Any error above this line is guaranteed not to have changed either endpoint.
        if source_available && !source_is_target {
            surface_interest_request(
                &source.boot_id,
                format!("client-shell-surface:{serial}:off"),
                false,
            )
            .map_err(|error| ActivationBeginError::Preflight(error.to_string()))?;
        }
        surface_interest_request(
            &target_lease.boot_id,
            format!("client-shell-surface:{serial}:on"),
            true,
        )
        .map_err(|error| ActivationBeginError::Preflight(error.to_string()))?;
        if let Some(target) = focus.as_ref() {
            focus_request(
                &target_lease.boot_id,
                format!("client-shell-focus:{serial}:1"),
                target,
            )
            .map_err(|error| ActivationBeginError::Preflight(error.to_string()))?;
        }

        let focus_acknowledged = focus.is_none();
        let source_background = source_available
            .then(|| source_background(shell, &source, &target_lease.endpoint_id, &resize))
            .flatten();
        Ok(Self {
            source_background,
            source,
            source_available,
            target: target_lease,
            focus,
            host_focused: shell.host_focus_baseline(),
            resize,
            phase: ActivationPhase::ActivatingTarget {
                request_id: format!("client-shell-surface:{serial}:on"),
                acknowledged_revision: None,
                focus_request_id: None,
                focus_request_target: None,
                focus_acknowledged,
                evidence: ActivationEvidence::default(),
            },
            deadline: now + ACTIVATION_TIMEOUT,
            started: now,
            warm_path,
            epoch: serial,
            next_focus_serial: 0,
            rollback_error: None,
            successor: None,
            input: BufferedInput::default(),
        })
    }

    pub(crate) fn start(
        mut self,
        endpoints: &mut EndpointRegistry,
    ) -> Result<Self, ActivationBeginError> {
        endpoints.freeze_input();
        // The target never waits for the source's release. The source's own ordered connection
        // keeps release before any later restore, and the target's activation reply alone
        // decides what is presented next.
        if self.source_available && self.source.endpoint_id != self.target.endpoint_id {
            release_source(
                &self.source,
                self.source_background.take(),
                endpoints,
                format!("client-shell-surface:{}:off", self.epoch),
            );
        }
        match self.start_target(endpoints) {
            Ok(()) => Ok(self),
            Err(error) => Err(ActivationBeginError::Partial {
                activation: Box::new(self),
                error,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn begin(
        shell: &crate::client::shell::ClientShellState,
        endpoints: &mut EndpointRegistry,
        target: ClientEndpointId,
        focus: Option<crate::client::shell::ClientEndpointFocusTarget>,
        resize: crate::protocol::ClientMessage,
        serial: u64,
        now: Instant,
    ) -> Result<Self, ActivationBeginError> {
        Self::prepare(
            shell,
            endpoints,
            target,
            focus,
            resize,
            serial,
            WarmPathMiss::NotKept,
            now,
        )?
        .start(endpoints)
    }

    pub(crate) fn abandon(&self, endpoints: &mut EndpointRegistry) {
        if self.started.elapsed() >= SLOW_ACTIVATION {
            self.report()
                .warn("endpoint activation abandoned", None, "abandoned");
        }
        endpoints.freeze_input();
        for lease in [&self.source, &self.target] {
            release_surface_best_effort(
                lease,
                endpoints,
                format!("client-shell-surface:{}:abandon", self.epoch),
            );
            if self.source.endpoint_id == self.target.endpoint_id {
                break;
            }
        }
    }

    pub(crate) fn target(&self) -> &ClientEndpointId {
        &self.target.endpoint_id
    }

    fn geometry(&self) -> crate::protocol::ClientSurfaceSize {
        resize_geometry(&self.resize).expect("activation resize was validated before construction")
    }

    /// The endpoint whose surface this phase is collecting, and what it has sent so far.
    fn collecting_mut(&mut self) -> Option<(&EndpointLease, &mut ActivationEvidence)> {
        match &mut self.phase {
            ActivationPhase::ActivatingTarget { evidence, .. } => Some((&self.target, evidence)),
            ActivationPhase::RestoringSource { evidence, .. } => Some((&self.source, evidence)),
            ActivationPhase::ReleasingTargetForRollback { .. } => None,
        }
    }

    /// The complete source command lane cannot safely cross source-off into a later presentation
    /// epoch. Other endpoint lanes are not part of this retirement.
    pub(crate) fn source_command_lane(&self) -> Option<&ClientEndpointId> {
        (self.source_available && self.source.endpoint_id != self.target.endpoint_id)
            .then_some(&self.source.endpoint_id)
    }

    pub(crate) fn can_retarget(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.target.endpoint_id == *endpoint_id
            && self.successor.is_none()
            && matches!(self.phase, ActivationPhase::ActivatingTarget { .. })
    }

    /// Replace an in-flight handoff with the latest endpoint-qualified intent. The current
    /// transaction is still reversed through target-off/source-on; the replacement is launched
    /// by the caller only after the source's coherent restoration commits.
    pub(crate) fn supersede(
        &mut self,
        endpoint_id: ClientEndpointId,
        target: Option<crate::client::shell::ClientEndpointFocusTarget>,
        endpoints: &mut EndpointRegistry,
    ) -> ActivationRollback {
        self.successor = Some(EndpointActivationIntent {
            endpoint_id,
            target,
        });
        // Source-on and target-off are already ordered and must finish before any replacement is
        // allowed to begin. Later rapid selections only replace the retained intent; they never
        // turn a safe restoration into an unavailable state or close a target before its
        // revocation deadline.
        if matches!(
            self.phase,
            ActivationPhase::RestoringSource { .. }
                | ActivationPhase::ReleasingTargetForRollback { .. }
        ) {
            return ActivationRollback::Pending;
        }
        self.rollback(
            endpoints,
            "endpoint handoff superseded by a newer selection".into(),
        )
    }

    pub(crate) fn accepts_endpoint(&self, endpoint_id: &ClientEndpointId, generation: u64) -> bool {
        (self.source_available
            && self.source.endpoint_id == *endpoint_id
            && self.source.generation == generation)
            || (self.target.endpoint_id == *endpoint_id && self.target.generation == generation)
    }

    pub(crate) fn involves_endpoint(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.source.endpoint_id == *endpoint_id || self.target.endpoint_id == *endpoint_id
    }

    pub(crate) fn accepts_response(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        boot_id: &str,
        request_id: &str,
    ) -> bool {
        match &self.phase {
            ActivationPhase::ActivatingTarget {
                request_id: expected,
                focus_request_id,
                ..
            } => {
                endpoint_matches(&self.target, endpoint_id, generation, boot_id)
                    && (expected == request_id || focus_request_id.as_deref() == Some(request_id))
            }
            ActivationPhase::ReleasingTargetForRollback {
                request_id: expected,
            } => {
                endpoint_matches(&self.target, endpoint_id, generation, boot_id)
                    && expected == request_id
            }
            ActivationPhase::RestoringSource {
                request_id: expected,
                ..
            } => {
                endpoint_matches(&self.source, endpoint_id, generation, boot_id)
                    && expected == request_id
            }
        }
    }

    pub(crate) fn expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }

    #[cfg(test)]
    pub(crate) fn receive_response(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        request_id: &str,
        data: &[u8],
        endpoints: &mut EndpointRegistry,
    ) -> SurfaceActivationProgress {
        let boot_id = if self.source.endpoint_id == *endpoint_id {
            self.source.boot_id.clone()
        } else {
            self.target.boot_id.clone()
        };
        self.receive_response_for_boot(
            endpoint_id,
            generation,
            &boot_id,
            request_id,
            data,
            endpoints,
        )
    }

    pub(crate) fn receive_response_for_boot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        boot_id: &str,
        request_id: &str,
        data: &[u8],
        endpoints: &mut EndpointRegistry,
    ) -> SurfaceActivationProgress {
        if !self.accepts_response(endpoint_id, generation, boot_id, request_id) {
            return SurfaceActivationProgress::Stale;
        }
        let result = match decode_endpoint_response(request_id, data) {
            Ok(result) => result,
            Err(error) => return SurfaceActivationProgress::Rejected(error.message),
        };
        match &mut self.phase {
            ActivationPhase::ActivatingTarget {
                request_id: surface_request_id,
                acknowledged_revision,
                ..
            } if surface_request_id == request_id => {
                match surface_set_revision(&result, true) {
                    Ok(revision) => *acknowledged_revision = Some(revision),
                    Err(message) => return SurfaceActivationProgress::Rejected(message),
                }
                self.progress()
            }
            ActivationPhase::ActivatingTarget {
                focus_request_id,
                focus_request_target,
                focus_acknowledged,
                ..
            } => {
                let Some(requested) = focus_request_target.clone() else {
                    return SurfaceActivationProgress::Stale;
                };
                if !focus_result_matches(Some(&requested), &result) {
                    return SurfaceActivationProgress::Rejected(
                        "endpoint focus returned an unexpected result".into(),
                    );
                }
                *focus_request_id = None;
                *focus_request_target = None;
                if self.focus != Some(requested) {
                    *focus_acknowledged = false;
                    if let Err(message) = self.send_latest_focus(endpoints) {
                        return SurfaceActivationProgress::Rejected(message);
                    }
                    return self.progress();
                }
                *focus_acknowledged = true;
                self.progress()
            }
            ActivationPhase::ReleasingTargetForRollback { .. } => {
                if let Err(message) = surface_set_revision(&result, false) {
                    return SurfaceActivationProgress::Rejected(message);
                }
                endpoints.set_surface_active(&self.target.endpoint_id, false);
                if !self.source_available {
                    return SurfaceActivationProgress::Rejected(
                        self.rollback_error.clone().unwrap_or_else(|| {
                            "the previous endpoint is no longer connected".into()
                        }),
                    );
                }
                if let Err(message) = self.start_source_restore(endpoints) {
                    return SurfaceActivationProgress::Rejected(message);
                }
                SurfaceActivationProgress::Pending
            }
            ActivationPhase::RestoringSource {
                acknowledged_revision,
                ..
            } => {
                match surface_set_revision(&result, true) {
                    Ok(revision) => *acknowledged_revision = Some(revision),
                    Err(message) => return SurfaceActivationProgress::Rejected(message),
                }
                self.progress()
            }
        }
    }

    pub(crate) fn receive_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        snapshot: &crate::protocol::ClientShellSnapshot,
    ) -> SurfaceActivationProgress {
        let Some((lease, evidence)) = self.collecting_mut() else {
            return SurfaceActivationProgress::Stale;
        };
        if !endpoint_matches(lease, endpoint_id, generation, &snapshot.boot_id)
            || snapshot.revision < lease.minimum_revision
        {
            return SurfaceActivationProgress::Stale;
        }
        evidence.record_snapshot(snapshot);
        self.progress()
    }

    pub(crate) fn receive_surface(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        surface: crate::protocol::PaneSurfaceFrame,
    ) -> SurfaceActivationProgress {
        let geometry = self.geometry();
        let Some((lease, evidence)) = self.collecting_mut() else {
            return SurfaceActivationProgress::Stale;
        };
        if !endpoint_matches(lease, endpoint_id, generation, &surface.boot_id) {
            return SurfaceActivationProgress::Stale;
        }
        if !surface_matches_geometry(&surface, geometry) {
            return SurfaceActivationProgress::Pending;
        }
        evidence.record_surface(surface);
        self.progress()
    }

    /// Returns whether the patch belonged to this activation's collected surface. Anything else
    /// is left to the committed shell.
    pub(crate) fn receive_surface_patch(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        patch: &crate::protocol::PaneSurfacePatch,
    ) -> bool {
        self.collecting_mut().is_some_and(|(lease, evidence)| {
            endpoint_matches(lease, endpoint_id, generation, &patch.boot_id)
                && evidence.record_patch(patch)
        })
    }

    /// Keep a presentation effect from the endpoint being activated for its commit. Effects from
    /// any other endpoint describe a frame that will not be shown and are dropped.
    pub(crate) fn receive_presentation_effect(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        effect: crate::protocol::ServerMessage,
    ) {
        if let Some((lease, evidence)) = self.collecting_mut() {
            if lease.endpoint_id == *endpoint_id && lease.generation == generation {
                evidence.record_effect(effect);
            }
        }
    }

    /// Keep pane input typed while frozen until the switch commits.
    pub(crate) fn buffer_input(&mut self, events: Vec<crate::protocol::ClientPaneInputEvent>) {
        self.input.push(events);
    }

    /// A same-endpoint navigation request replaces the desired target but never joins the
    /// in-flight focus RPC. Once that request resolves, `send_latest_focus` sends only the most
    /// recent desired target.
    pub(crate) fn retarget(
        &mut self,
        focus: Option<crate::client::shell::ClientEndpointFocusTarget>,
        endpoints: &mut EndpointRegistry,
    ) -> Result<(), String> {
        self.focus = focus;
        let ActivationPhase::ActivatingTarget {
            focus_request_id,
            focus_acknowledged,
            ..
        } = &mut self.phase
        else {
            return Ok(());
        };
        *focus_acknowledged = self.focus.is_none() && focus_request_id.is_none();
        self.send_latest_focus(endpoints)
    }

    pub(crate) fn update_resize(
        &mut self,
        resize: crate::protocol::ClientMessage,
        endpoints: &mut EndpointRegistry,
    ) -> Result<(), String> {
        resize_geometry(&resize)
            .ok_or_else(|| "endpoint activation did not include a surface resize".to_owned())?;
        self.resize = resize.clone();
        let Some((lease, evidence)) = self.collecting_mut() else {
            return Ok(());
        };
        evidence.invalidate_surface();
        let destination = lease.endpoint_id.clone();
        if endpoints.send_to(&destination, &resize) != EndpointSendOutcome::Sent {
            return Err("pending endpoint resize could not be sent".into());
        }
        Ok(())
    }

    pub(crate) fn update_host_focus(
        &mut self,
        focused: bool,
        endpoints: &mut EndpointRegistry,
    ) -> Result<(), String> {
        self.host_focused = focused;
        // The endpoint streams any repaint this causes as patches, which follow the collected
        // surface, so the evidence stays valid.
        let Some((lease, _)) = self.collecting_mut() else {
            return Ok(());
        };
        let destination = lease.endpoint_id.clone();
        if endpoints.send_to(
            &destination,
            &crate::protocol::ClientMessage::ClientShellFocus { focused },
        ) != EndpointSendOutcome::Sent
        {
            return Err("pending endpoint focus baseline could not be sent".into());
        }
        Ok(())
    }

    /// Endpoints keep the host theme per connection even while their surface is inactive, so
    /// both sides learn it now and either can be committed without a replay.
    pub(crate) fn update_host_theme(
        &mut self,
        update: crate::protocol::ClientHostThemeUpdate,
        endpoints: &mut EndpointRegistry,
    ) {
        let message = crate::protocol::ClientMessage::ClientShellHostTheme { update };
        for lease in [&self.target, &self.source] {
            if endpoints.accepts(&lease.endpoint_id, lease.generation) {
                let _ = endpoints.send_to(&lease.endpoint_id, &message);
            }
            if self.source.endpoint_id == self.target.endpoint_id {
                break;
            }
        }
    }

    /// Losing the source revokes its surface and removes the rollback destination; it must not
    /// cancel a healthy target. Losing the target restores the source when it is still available.
    pub(crate) fn endpoint_disconnected(
        &mut self,
        endpoints: &mut EndpointRegistry,
        endpoint_id: &ClientEndpointId,
        error: String,
    ) -> ActivationRollback {
        let report = self.report();
        let rollback = self.lose_endpoint(endpoints, endpoint_id, error);
        report.warn(
            "endpoint activation lost an endpoint",
            self.logged_error(&rollback),
            self.rollback_outcome(&rollback),
        );
        rollback
    }

    fn lose_endpoint(
        &mut self,
        endpoints: &mut EndpointRegistry,
        endpoint_id: &ClientEndpointId,
        error: String,
    ) -> ActivationRollback {
        self.rollback_error = Some(error.clone());
        if self.target.endpoint_id == *endpoint_id && self.source.endpoint_id != *endpoint_id {
            return match self.phase {
                ActivationPhase::RestoringSource { .. } => ActivationRollback::Pending,
                _ => match self.start_source_restore(endpoints) {
                    Ok(()) => ActivationRollback::Pending,
                    Err(restore_error) => ActivationRollback::Unavailable(format!(
                        "{error}; source endpoint could not be restored safely: {restore_error}"
                    )),
                },
            };
        }
        if self.source.endpoint_id != *endpoint_id {
            return ActivationRollback::Unavailable(error);
        }
        self.source_available = false;
        match self.phase {
            ActivationPhase::ActivatingTarget { .. }
                if self.source.endpoint_id != self.target.endpoint_id =>
            {
                ActivationRollback::Pending
            }
            ActivationPhase::ActivatingTarget { .. } => {
                match self.start_target_release(endpoints) {
                    Ok(()) => ActivationRollback::Pending,
                    Err(release_error) => ActivationRollback::Unavailable(format!(
                        "{error}; target endpoint could not be released safely: {release_error}"
                    )),
                }
            }
            ActivationPhase::ReleasingTargetForRollback { .. } => ActivationRollback::Pending,
            ActivationPhase::RestoringSource { .. } => ActivationRollback::Unavailable(error),
        }
    }

    pub(crate) fn rollback(
        &mut self,
        endpoints: &mut EndpointRegistry,
        error: String,
    ) -> ActivationRollback {
        let report = self.report();
        let rollback = self.start_rollback(endpoints, error);
        report.warn(
            "endpoint activation rolled back",
            self.logged_error(&rollback),
            self.rollback_outcome(&rollback),
        );
        rollback
    }

    fn start_rollback(
        &mut self,
        endpoints: &mut EndpointRegistry,
        error: String,
    ) -> ActivationRollback {
        self.rollback_error = Some(error.clone());
        let result = match self.phase {
            ActivationPhase::ActivatingTarget { .. } => self.start_target_release(endpoints),
            ActivationPhase::ReleasingTargetForRollback { .. } => {
                // The target may have observed target-on or target-off. Closing this transport
                // is the only safe local revocation when target-off is not acknowledged.
                endpoints.fail(
                    &self.target.endpoint_id,
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "endpoint did not acknowledge surface revocation",
                    ),
                );
                if !self.source_available {
                    return ActivationRollback::Unavailable(format!(
                        "{error}; the target connection was closed because no presentation owner could be proven"
                    ));
                }
                self.start_source_restore(endpoints)
            }
            ActivationPhase::RestoringSource { .. } => {
                return ActivationRollback::Unavailable(format!(
                    "{error}; source endpoint could not be restored"
                ));
            }
        };
        match result {
            Ok(()) => ActivationRollback::Pending,
            Err(rollback_error) => ActivationRollback::Unavailable(format!(
                "{error}; source endpoint could not be restored safely: {rollback_error}"
            )),
        }
    }

    /// Commit the collected frame. Input opens with this frame: nothing about the endpoint's
    /// presentation is still outstanding once its snapshot, surface and replies have arrived.
    pub(crate) fn complete(
        &mut self,
        shell: &mut crate::client::shell::ClientShellState,
        endpoints: &mut EndpointRegistry,
    ) -> Result<CommittedActivation, String> {
        let committed = self.commit(shell, endpoints);
        match committed.as_ref().map(|committed| &committed.completion) {
            Ok(ActivationCompletion::Activated) => {
                if self.started.elapsed() >= SLOW_ACTIVATION {
                    self.report()
                        .warn("slow endpoint activation", None, "activated");
                }
            }
            Ok(ActivationCompletion::RestoredSource { error, .. }) => self.report().warn(
                "endpoint activation restored the previous endpoint",
                Some(error),
                "restored_source",
            ),
            Err(error) => self.report().warn(
                "endpoint activation could not commit",
                Some(error),
                "unavailable",
            ),
        }
        committed
    }

    fn commit(
        &mut self,
        shell: &mut crate::client::shell::ClientShellState,
        endpoints: &mut EndpointRegistry,
    ) -> Result<CommittedActivation, String> {
        let geometry = self.geometry();
        let (lease, evidence, acknowledgement_revision, completion) = match &mut self.phase {
            ActivationPhase::ActivatingTarget {
                evidence,
                acknowledged_revision,
                ..
            } => (
                &self.target,
                evidence,
                *acknowledged_revision,
                ActivationCompletion::Activated,
            ),
            ActivationPhase::RestoringSource {
                evidence,
                acknowledged_revision,
                ..
            } => (
                &self.source,
                evidence,
                *acknowledged_revision,
                ActivationCompletion::RestoredSource {
                    error: self
                        .rollback_error
                        .clone()
                        .unwrap_or_else(|| "endpoint handoff was rolled back".into()),
                    successor: self.successor.clone(),
                },
            ),
            ActivationPhase::ReleasingTargetForRollback { .. } => {
                return Err("endpoint activation completed in an invalid phase".into());
            }
        };
        let surface = coherent_completion_surface(
            shell,
            lease,
            evidence,
            acknowledgement_revision,
            geometry,
        )?;
        endpoints.set_surface_active(&lease.endpoint_id, true);
        shell.set_endpoint_status(&lease.endpoint_id, ClientEndpointStatus::Online);
        if !shell.endpoint_projection_available(&lease.endpoint_id)
            || !endpoints.set_active(&lease.endpoint_id)
        {
            return Err("endpoint became unavailable during activation".into());
        }
        let activated = shell.activate_endpoint_projection(&lease.endpoint_id);
        debug_assert!(activated, "preflighted endpoint projection must activate");
        debug_assert!(shell.endpoint_is_active(endpoints.active_id()));
        shell.set_pane_surface(surface);
        let effects = std::mem::take(&mut evidence.effects);
        let (endpoint_id, generation) = (lease.endpoint_id.clone(), lease.generation);
        // Input typed during a switch that fell back was meant for the endpoint the user was
        // leaving, not for the one that is shown again.
        let input = match completion {
            ActivationCompletion::Activated => self.input.take(),
            ActivationCompletion::RestoredSource { .. } => Vec::new(),
        };
        Ok(CommittedActivation {
            completion,
            endpoint_id,
            generation,
            effects,
            input,
        })
    }

    /// An unavailable outcome carries why the rollback itself failed, on top of the original error.
    fn logged_error<'a>(&'a self, rollback: &'a ActivationRollback) -> Option<&'a str> {
        match rollback {
            ActivationRollback::Unavailable(message) => Some(message),
            ActivationRollback::Pending => self.rollback_error.as_deref(),
        }
    }

    fn rollback_outcome(&self, rollback: &ActivationRollback) -> &'static str {
        match rollback {
            ActivationRollback::Pending => self.phase.name(),
            ActivationRollback::Unavailable(_) => "unavailable",
        }
    }

    /// What this activation has received so far. Built only for a slow or failed switch.
    fn report(&self) -> ActivationReport {
        let (acknowledged_revision, focus_acknowledged, focus_request_id, evidence) =
            match &self.phase {
                ActivationPhase::ActivatingTarget {
                    acknowledged_revision,
                    focus_acknowledged,
                    focus_request_id,
                    evidence,
                    ..
                } => (
                    *acknowledged_revision,
                    Some(*focus_acknowledged),
                    focus_request_id.clone(),
                    Some(evidence),
                ),
                ActivationPhase::RestoringSource {
                    acknowledged_revision,
                    evidence,
                    ..
                } => (*acknowledged_revision, None, None, Some(evidence)),
                ActivationPhase::ReleasingTargetForRollback { .. } => (None, None, None, None),
            };
        let geometry = self.geometry();
        ActivationReport {
            endpoint: self.target.endpoint_id.storage_key(),
            source: self.source.endpoint_id.storage_key(),
            epoch: self.epoch,
            generation: self.target.generation,
            elapsed_ms: self.started.elapsed().as_millis(),
            phase: self.phase.name(),
            warm_path: self.warm_path,
            acknowledged_revision,
            focus_acknowledged,
            focus_request_id,
            snapshot_revision: evidence.and_then(|evidence| evidence.snapshot_revision),
            focused_workspace_id: evidence
                .and_then(|evidence| evidence.focused_workspace_id.clone()),
            surface: evidence
                .and_then(|evidence| evidence.surface.as_ref())
                .map(|surface| {
                    format!(
                        "{}/{} {}x{}",
                        surface.projection_revision,
                        surface.surface_revision,
                        surface.frame.width,
                        surface.frame.height
                    )
                }),
            geometry: format!("{}x{}", geometry.cols, geometry.rows),
        }
    }

    fn start_target(&mut self, endpoints: &mut EndpointRegistry) -> Result<(), String> {
        let request_id = format!("client-shell-surface:{}:on", self.epoch);
        self.deadline = Instant::now() + ACTIVATION_TIMEOUT;
        // A transport may fail after writing any baseline or surface message. Enter the target
        // phase first so every uncertain target write is reversed through target-off before
        // source restoration is considered.
        self.phase = ActivationPhase::ActivatingTarget {
            request_id: request_id.clone(),
            acknowledged_revision: None,
            focus_request_id: None,
            focus_request_target: None,
            focus_acknowledged: self.focus.is_none(),
            evidence: ActivationEvidence::default(),
        };
        send_surface_activation(
            endpoints,
            &self.target,
            request_id,
            &self.resize,
            self.host_focused,
        )?;

        // From this point the target may have processed surface.set(true). Optional navigation
        // is serialized through one coalescing focus lane.
        self.send_latest_focus(endpoints)
    }

    fn start_target_release(&mut self, endpoints: &mut EndpointRegistry) -> Result<(), String> {
        let request_id = format!("client-shell-surface:{}:rollback-target-off", self.epoch);
        let request = surface_interest_request(&self.target.boot_id, request_id.clone(), false)
            .map_err(|error| error.to_string())?;
        // Set the rollback phase before the potentially observed target-off write.
        self.phase = ActivationPhase::ReleasingTargetForRollback { request_id };
        self.deadline = Instant::now() + ACTIVATION_TIMEOUT;
        if endpoints.send_to(&self.target.endpoint_id, &request) != EndpointSendOutcome::Sent {
            return Err("target endpoint release could not be sent".into());
        }
        Ok(())
    }

    fn start_source_restore(&mut self, endpoints: &mut EndpointRegistry) -> Result<(), String> {
        let request_id = format!("client-shell-surface:{}:rollback-source-on", self.epoch);
        // Source baseline writes can also be observed before their send reports an error.
        self.phase = ActivationPhase::RestoringSource {
            request_id: request_id.clone(),
            acknowledged_revision: None,
            evidence: ActivationEvidence::default(),
        };
        self.deadline = Instant::now() + ACTIVATION_TIMEOUT;
        send_surface_activation(
            endpoints,
            &self.source,
            request_id,
            &self.resize,
            self.host_focused,
        )
    }

    fn send_latest_focus(&mut self, endpoints: &mut EndpointRegistry) -> Result<(), String> {
        let desired = self.focus.clone();
        let Some(desired) = desired else {
            if let ActivationPhase::ActivatingTarget {
                focus_request_id,
                focus_acknowledged,
                ..
            } = &mut self.phase
            {
                if focus_request_id.is_none() {
                    *focus_acknowledged = true;
                }
            }
            return Ok(());
        };
        if !matches!(
            &self.phase,
            ActivationPhase::ActivatingTarget {
                focus_request_id: None,
                ..
            }
        ) {
            return Ok(());
        }
        let request_id = self
            .next_focus_request_id()
            .expect("desired focus creates a request id");
        if let ActivationPhase::ActivatingTarget {
            focus_request_id,
            focus_request_target,
            focus_acknowledged,
            ..
        } = &mut self.phase
        {
            *focus_acknowledged = false;
            *focus_request_id = Some(request_id.clone());
            *focus_request_target = Some(desired.clone());
        }
        let request = focus_request(&self.target.boot_id, request_id, &desired)
            .map_err(|error| error.to_string())?;
        if endpoints.send_to(&self.target.endpoint_id, &request) != EndpointSendOutcome::Sent {
            return Err("endpoint focus could not be sent".into());
        }
        Ok(())
    }

    fn next_focus_request_id(&mut self) -> Option<String> {
        self.focus.as_ref()?;
        self.next_focus_serial = self.next_focus_serial.saturating_add(1);
        Some(format!(
            "client-shell-focus:{}:{}",
            self.epoch, self.next_focus_serial
        ))
    }

    fn progress(&self) -> SurfaceActivationProgress {
        match &self.phase {
            ActivationPhase::ActivatingTarget {
                acknowledged_revision,
                focus_acknowledged,
                evidence,
                ..
            } if acknowledged_revision.is_some_and(|revision| {
                *focus_acknowledged
                    && evidence
                        .coherent_surface(revision, self.geometry())
                        .is_some_and(|surface| self.target_matches(surface))
            }) =>
            {
                SurfaceActivationProgress::Ready
            }
            ActivationPhase::RestoringSource {
                acknowledged_revision,
                evidence,
                ..
            } if acknowledged_revision.is_some_and(|revision| {
                evidence
                    .coherent_surface(revision, self.geometry())
                    .is_some()
            }) =>
            {
                SurfaceActivationProgress::Ready
            }
            _ => SurfaceActivationProgress::Pending,
        }
    }

    fn target_matches(&self, surface: &crate::protocol::PaneSurfaceFrame) -> bool {
        let evidence = match &self.phase {
            ActivationPhase::ActivatingTarget { evidence, .. } => evidence,
            _ => return false,
        };
        match &self.focus {
            Some(crate::client::shell::ClientEndpointFocusTarget::Pane(pane_id)) => {
                evidence.focused_pane_id.as_deref() == Some(pane_id)
                    && surface
                        .panes
                        .iter()
                        .any(|pane| pane.focused && &pane.pane_id == pane_id)
            }
            Some(crate::client::shell::ClientEndpointFocusTarget::Tab(tab_id)) => {
                evidence.focused_tab_id.as_deref() == Some(tab_id)
            }
            Some(crate::client::shell::ClientEndpointFocusTarget::Workspace(workspace_id)) => {
                evidence.focused_workspace_id.as_deref() == Some(workspace_id)
            }
            None => true,
        }
    }
}

/// One activation's state for the client log. `surface` is `projection/surface WxH`, to compare
/// against the expected `geometry`.
struct ActivationReport {
    endpoint: String,
    source: String,
    epoch: u64,
    generation: u64,
    elapsed_ms: u128,
    phase: &'static str,
    warm_path: WarmPathMiss,
    acknowledged_revision: Option<u64>,
    focus_acknowledged: Option<bool>,
    focus_request_id: Option<String>,
    snapshot_revision: Option<u64>,
    focused_workspace_id: Option<String>,
    surface: Option<String>,
    geometry: String,
}

impl ActivationReport {
    fn warn(self, message: &str, error: Option<&str>, outcome: &str) {
        tracing::warn!(
            endpoint = %self.endpoint,
            source = %self.source,
            epoch = self.epoch,
            generation = self.generation,
            elapsed_ms = self.elapsed_ms,
            phase = self.phase,
            warm_path = %self.warm_path,
            acknowledged_revision = ?self.acknowledged_revision,
            focus_acknowledged = ?self.focus_acknowledged,
            focus_request_id = ?self.focus_request_id,
            snapshot_revision = ?self.snapshot_revision,
            focused_workspace = ?self.focused_workspace_id,
            surface = ?self.surface,
            geometry = %self.geometry,
            error = ?error,
            outcome,
            "{message}"
        );
    }
}

#[cfg(test)]
#[path = "activation_tests.rs"]
mod tests;
