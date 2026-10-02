//! Optional screens of tabs a client has not shown yet, so its first switch to one can show a
//! placeholder at once. These frames are not part of the connection's surface stream and never
//! move a decoder baseline.

use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};

use super::{ClientMessage, PaneSurfaceFrame, ServerMessage, MAX_FRAME_SIZE};

pub(crate) const CAPABILITY: &str = "tab_screen_prefetch";
/// Client-to-server: a JSON list of tab IDs whose screens the client wants.
pub(crate) const REQUEST_KIND: &str = "endpoint.tab-screens.request.v1";
/// Server-to-client: one requested tab's screen, rendered as if this connection focused it.
pub(crate) const MESSAGE_KIND: &str = "endpoint.tab-screen.v1";

pub(crate) fn request(tab_ids: &[String]) -> ClientMessage {
    ClientMessage::EndpointControl {
        kind: REQUEST_KIND.into(),
        data: serde_json::to_string(tab_ids).expect("strings always encode"),
    }
}

pub(crate) fn decode_request(data: &str) -> Result<Vec<String>, String> {
    serde_json::from_str(data).map_err(|error| format!("invalid tab screen request: {error}"))
}

// This binary layout is frozen with MESSAGE_KIND, independently of the v1 core.
pub(crate) fn message(tab_id: &str, surface: &PaneSurfaceFrame) -> ServerMessage {
    let bytes = bincode::serde::encode_to_vec((tab_id, surface), bincode::config::standard())
        .expect("pane surfaces always encode");
    ServerMessage::EndpointControl {
        kind: MESSAGE_KIND.into(),
        data: STANDARD_NO_PAD.encode(bytes),
    }
}

pub(crate) fn decode(data: &str) -> Result<(String, PaneSurfaceFrame), String> {
    let bytes = STANDARD_NO_PAD
        .decode(data)
        .map_err(|error| format!("invalid tab screen: {error}"))?;
    let (screen, consumed) = bincode::serde::decode_from_slice(
        &bytes,
        bincode::config::standard().with_limit::<MAX_FRAME_SIZE>(),
    )
    .map_err(|error| format!("invalid tab screen: {error}"))?;
    if consumed != bytes.len() {
        return Err("trailing tab screen bytes".into());
    }
    Ok(screen)
}
