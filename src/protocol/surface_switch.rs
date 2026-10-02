//! Optional per-tab surface baselines. A tab switch parks the outgoing surface on both ends of
//! the connection, so a later return to that tab can arrive as a delta against it.
//!
//! A switch is a header frame followed, in the same write, by an ordinary pane surface or
//! surface delta against the baseline the header made current.

use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};

use super::{ServerMessage, MAX_FRAME_SIZE};

pub(crate) const CAPABILITY: &str = "surface_tab_baselines";
pub(crate) const MESSAGE_KIND: &str = "endpoint.surface-switch.v1";
/// Parked baselines each hold a whole cell grid on both ends, so their number is bounded.
pub(crate) const MAX_PARKED: usize = 8;

// This binary layout is frozen with MESSAGE_KIND, independently of the v1 core.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SurfaceSwitch {
    /// Surface revisions of every parked baseline after this switch, oldest first.
    pub(crate) keep: Vec<u64>,
    /// The parked baseline that becomes current for the surface that follows.
    pub(crate) restore: Option<u64>,
}

pub(crate) fn message(switch: &SurfaceSwitch) -> ServerMessage {
    let bytes = bincode::serde::encode_to_vec(switch, bincode::config::standard())
        .expect("integers always encode");
    ServerMessage::EndpointControl {
        kind: MESSAGE_KIND.into(),
        data: STANDARD_NO_PAD.encode(bytes),
    }
}

pub(crate) fn decode(data: &str) -> Result<SurfaceSwitch, String> {
    let bytes = STANDARD_NO_PAD
        .decode(data)
        .map_err(|error| format!("invalid surface switch: {error}"))?;
    let (switch, consumed) = bincode::serde::decode_from_slice(
        &bytes,
        bincode::config::standard().with_limit::<MAX_FRAME_SIZE>(),
    )
    .map_err(|error| format!("invalid surface switch: {error}"))?;
    if consumed != bytes.len() {
        return Err("trailing surface switch bytes".into());
    }
    Ok(switch)
}
