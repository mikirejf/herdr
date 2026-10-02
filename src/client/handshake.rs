use std::io;
#[cfg(unix)]
use std::io::IsTerminal as _;
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;
#[cfg(windows)]
use tracing::debug;
use tracing::info;

use crate::ipc::LocalStream;
use crate::protocol::endpoint::{
    EndpointClientHello, EndpointServerWelcome, BLOB_CODEC_V1, ENDPOINT_HELLO_KIND,
    ENDPOINT_PROTOCOL_GENERATION, ENDPOINT_WELCOME_KIND, INPUT_CODEC_V1, SNAPSHOT_CODEC_V1,
    SURFACE_ACK_CAPABILITY, SURFACE_CODEC_V1,
};
use crate::protocol::{
    self, ClientMessage, RenderEncoding, ServerMessage, MAX_FRAME_SIZE, PROTOCOL_VERSION,
};

#[cfg(unix)]
use super::terminal_setup::is_ssh_session;
use super::{shell, ClientError};

/// Time to wait for the server's Welcome reply during the handshake.
///
/// A local client talks to an already-connected server, so 5s is plenty. The
/// remote bridge client (`herdr --remote`) sits behind a fresh per-attach ssh
/// connection whose cold-connect (TCP + key exchange + auth) happens inside this
/// window; on a high-latency link that easily exceeds 5s, so it gets a far
/// larger budget. See issue #753.
pub(super) const LOCAL_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const REMOTE_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) fn is_remote_client_process() -> bool {
    std::env::var(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR).is_ok()
}

pub(super) fn client_shell_keybinding_source() -> shell::ClientShellKeybindingSource {
    match std::env::var(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR)
        .ok()
        .as_deref()
    {
        Some("server") => shell::ClientShellKeybindingSource::Endpoint,
        Some(_) => shell::ClientShellKeybindingSource::RemoteLocal,
        None => shell::ClientShellKeybindingSource::Local,
    }
}

pub(super) fn handshake_read_timeout() -> Duration {
    if is_remote_client_process() {
        return REMOTE_HANDSHAKE_READ_TIMEOUT;
    }
    LOCAL_HANDSHAKE_READ_TIMEOUT
}

#[cfg(any(unix, test))]
pub(super) fn direct_graphics_profile_values(
    term_program: &str,
    term: &str,
    kitty_window: bool,
    blocked_transport: bool,
    terminals: bool,
) -> bool {
    let supported = term_program.eq_ignore_ascii_case("ghostty")
        || term_program.eq_ignore_ascii_case("wezterm")
        || matches!(term, "xterm-ghostty" | "xterm-kitty" | "xterm-wezterm")
        || kitty_window;
    supported && !blocked_transport && terminals
}

/// Server-owned files require a shared filesystem; the host terminal profile alone
/// cannot establish that for a saved SSH endpoint.
fn direct_graphics_capability(
    local_transport: bool,
    exact_cell_size: bool,
    cell_size: (u32, u32),
    profile_allowed: bool,
) -> bool {
    local_transport && exact_cell_size && cell_size.0 > 0 && cell_size.1 > 0 && profile_allowed
}

#[cfg(unix)]
fn direct_graphics_profile_allowed() -> bool {
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let term = std::env::var("TERM").unwrap_or_default();
    direct_graphics_profile_values(
        &term_program,
        &term,
        std::env::var_os("KITTY_WINDOW_ID").is_some(),
        is_remote_client_process()
            || is_ssh_session()
            || std::env::var_os("TMUX").is_some()
            || std::env::var_os("STY").is_some(),
        io::stdin().is_terminal() && io::stdout().is_terminal(),
    )
}

#[cfg(not(unix))]
fn direct_graphics_profile_allowed() -> bool {
    false
}

#[cfg(windows)]
fn set_handshake_recv_timeout(
    stream: &LocalStream,
    timeout: Option<Duration>,
    context: &'static str,
) -> Result<(), ClientError> {
    match stream.set_recv_timeout(timeout) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::Unsupported => {
            debug!(err = %err, context, "client socket receive timeout unavailable");
            Ok(())
        }
        Err(err) => Err(ClientError::ConnectionFailed(err)),
    }
}

#[cfg(not(windows))]
fn set_handshake_recv_timeout(
    stream: &LocalStream,
    timeout: Option<Duration>,
    _context: &'static str,
) -> Result<(), ClientError> {
    stream
        .set_recv_timeout(timeout)
        .map_err(ClientError::ConnectionFailed)
}

/// Byte acks bound what an SSH channel can queue ahead of input echo. A local socket has no such
/// hidden buffer, so local connections skip the ack traffic.
fn surface_ack_requested(local_transport: bool) -> bool {
    !local_transport
}

/// The server advertises `surface_ack` to every client but enforces it only for a hello that
/// asked for it, so the capability stays on the connection only when this client asked too.
fn agreed_capabilities(mut capabilities: Vec<String>, surface_ack_requested: bool) -> Vec<String> {
    if !surface_ack_requested {
        capabilities.retain(|capability| capability != SURFACE_ACK_CAPABILITY);
    }
    capabilities
}

#[derive(Debug)]
pub(super) struct HandshakeResult {
    pub(super) encoding: RenderEncoding,
    pub(super) endpoint_methods: Option<Vec<String>>,
    pub(super) endpoint_capabilities: Option<Vec<String>>,
}

pub(crate) fn probe_endpoint_negotiation(
    stream: &mut LocalStream,
) -> io::Result<super::endpoint::EndpointNegotiation> {
    let handshake = do_handshake(
        stream,
        80,
        24,
        0,
        0,
        false,
        Some(crate::protocol::ClientSurfaceSize { cols: 80, rows: 24 }),
        false,
        false,
        false,
        false, // This probe uses an SSH bridge, not a shared filesystem.
    )
    .map_err(io::Error::other)?;
    Ok(super::endpoint::EndpointNegotiation::new(
        handshake.endpoint_methods.unwrap_or_default(),
        handshake.endpoint_capabilities.unwrap_or_default(),
    ))
}

/// Performs the client→server handshake.
///
/// Direct terminal clients retain the same-install private protocol. Client-owned
/// shells use the stable endpoint generation and negotiate whole codecs without
/// comparing Herdr build versions. `local_transport` means the endpoint shares the
/// client's filesystem, not merely that its bridge exposes a local socket.
pub(super) fn do_handshake(
    stream: &mut LocalStream,
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    exact_cell_size: bool,
    shell_surface_size: Option<crate::protocol::ClientSurfaceSize>,
    endpoint_keybindings: bool,
    mouse_capture: bool,
    surface_active: bool,
    local_transport: bool,
) -> Result<HandshakeResult, ClientError> {
    stream
        .set_nonblocking(false)
        .map_err(ClientError::ConnectionFailed)?;

    let endpoint_shell = shell_surface_size.is_some();
    let hello = if let Some(surface_size) = shell_surface_size {
        let hello = EndpointClientHello {
            generation: ENDPOINT_PROTOCOL_GENERATION,
            cell_width_px,
            cell_height_px,
            surface_size,
            pixel_mouse: exact_cell_size && cfg!(unix),
            direct_graphics: direct_graphics_capability(
                local_transport,
                exact_cell_size,
                (cell_width_px, cell_height_px),
                direct_graphics_profile_allowed(),
            ),
            endpoint_keybindings,
            mouse_capture,
            surface_active,
            surface_reuse: true,
            surface_delta: true,
            surface_scroll: true,
            surface_tab_baselines: true,
            surface_ack: surface_ack_requested(local_transport),
            snapshot_codecs: vec![SNAPSHOT_CODEC_V1.into()],
            surface_codecs: vec![SURFACE_CODEC_V1.into()],
            input_codecs: vec![INPUT_CODEC_V1.into()],
            blob_codecs: vec![BLOB_CODEC_V1.into()],
        };
        ClientMessage::EndpointControl {
            kind: ENDPOINT_HELLO_KIND.into(),
            data: serde_json::to_string(&hello).map_err(|error| {
                ClientError::ConnectionFailed(io::Error::new(io::ErrorKind::InvalidData, error))
            })?,
        }
    } else {
        ClientMessage::TerminalHello {
            version: PROTOCOL_VERSION,
            cols,
            rows,
            cell_width_px,
            cell_height_px,
            pixel_mouse: exact_cell_size && cfg!(unix),
        }
    };
    protocol::write_message(stream, &hello)
        .map_err(|e| ClientError::ConnectionFailed(io::Error::other(e.to_string())))?;

    let read_timeout = if endpoint_shell && !surface_active {
        REMOTE_HANDSHAKE_READ_TIMEOUT
    } else {
        handshake_read_timeout()
    };
    set_handshake_recv_timeout(
        stream,
        Some(read_timeout),
        "client handshake read timeout unavailable",
    )?;
    let welcome: ServerMessage = protocol::read_message(stream, MAX_FRAME_SIZE)?;
    set_handshake_recv_timeout(
        stream,
        None,
        "failed to clear client handshake read timeout",
    )?;

    if endpoint_shell {
        let ServerMessage::EndpointControl { kind, data } = welcome else {
            return Err(ClientError::Protocol(protocol::FramingError::Io(
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "server does not support the stable Herdr endpoint protocol; update this machine",
                ),
            )));
        };
        if kind != ENDPOINT_WELCOME_KIND {
            return Err(ClientError::Protocol(protocol::FramingError::Io(
                io::Error::new(io::ErrorKind::InvalidData, "expected endpoint welcome"),
            )));
        }
        let welcome: EndpointServerWelcome = serde_json::from_str(&data).map_err(|error| {
            ClientError::Protocol(protocol::FramingError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid endpoint welcome: {error}"),
            )))
        })?;
        if let Some(error) = welcome.error {
            return Err(ClientError::HandshakeRejected {
                version: welcome.generation,
                error: error.message,
            });
        }
        if welcome.generation != ENDPOINT_PROTOCOL_GENERATION
            || welcome.snapshot_codec != SNAPSHOT_CODEC_V1
            || welcome.surface_codec != SURFACE_CODEC_V1
            || welcome.input_codec != INPUT_CODEC_V1
            || welcome.blob_codec != BLOB_CODEC_V1
        {
            return Err(ClientError::HandshakeRejected {
                version: welcome.generation,
                error: "server has no compatible endpoint core; update this machine".into(),
            });
        }
        info!(
            generation = welcome.generation,
            server_version = %welcome.server_version,
            "endpoint handshake succeeded"
        );
        return Ok(HandshakeResult {
            encoding: RenderEncoding::SemanticFrame,
            endpoint_methods: Some(welcome.methods),
            endpoint_capabilities: Some(agreed_capabilities(
                welcome.capabilities,
                surface_ack_requested(local_transport),
            )),
        });
    }

    match welcome {
        ServerMessage::Welcome {
            version,
            encoding,
            error,
        } => {
            if let Some(error) = error {
                return Err(ClientError::HandshakeRejected { version, error });
            }
            info!(version, ?encoding, "handshake succeeded");
            Ok(HandshakeResult {
                encoding,
                endpoint_methods: None,
                endpoint_capabilities: None,
            })
        }
        _ => Err(ClientError::Protocol(protocol::FramingError::Io(
            io::Error::new(io::ErrorKind::InvalidData, "expected Welcome message"),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_graphics_requires_local_transport_even_on_supported_host_terminal() {
        let supported = direct_graphics_profile_values("ghostty", "", false, false, true);
        assert!(supported);
        assert!(direct_graphics_capability(true, true, (8, 16), supported));
        assert!(!direct_graphics_capability(false, true, (8, 16), supported));
        assert!(!direct_graphics_capability(true, false, (8, 16), supported));
        assert!(!direct_graphics_capability(true, true, (0, 16), supported));
        assert!(!direct_graphics_capability(true, true, (8, 0), supported));
        assert!(!direct_graphics_capability(true, true, (8, 16), false));
    }

    #[cfg(unix)]
    fn stream_pair(name: &str) -> (LocalStream, LocalStream, std::path::PathBuf) {
        use interprocess::local_socket::traits::Listener as _;
        let _ = name;
        let path = std::path::PathBuf::from("/tmp").join(format!(
            "hh{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let listener = crate::ipc::bind_private_local_listener(&path).unwrap();
        let accepting = std::thread::spawn(move || listener.accept().unwrap());
        let client = crate::ipc::connect_local_stream(&path).unwrap();
        (client, accepting.join().unwrap(), path)
    }

    /// Runs the client handshake against the real server handshake and returns whether the client
    /// will ack, plus the server's flow counts for the connection.
    #[cfg(unix)]
    fn negotiate_surface_ack(local_transport: bool) -> (bool, Option<(u64, u64)>) {
        use crate::server::client_transport::{handle_client_handshake, ServerEvent};
        let (mut client, server, path) = stream_pair("surface-ack-negotiation");
        let (events, mut event_rx) = tokio::sync::mpsc::channel(4);
        let quit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_quit = quit.clone();
        let handle =
            std::thread::spawn(move || handle_client_handshake(server, 1, &events, &server_quit));
        let handshake = do_handshake(
            &mut client,
            80,
            24,
            8,
            16,
            false,
            Some(crate::protocol::ClientSurfaceSize { cols: 80, rows: 24 }),
            false,
            false,
            true,
            local_transport,
        )
        .unwrap();
        let flow = match event_rx.blocking_recv().unwrap() {
            ServerEvent::ClientShellConnected { writer, .. } => writer.test_flow_counts(),
            other => panic!("expected ClientShellConnected, got {other:?}"),
        };
        let acks = super::super::endpoint::EndpointNegotiation::new(
            handshake.endpoint_methods.unwrap_or_default(),
            handshake.endpoint_capabilities.unwrap_or_default(),
        )
        .supports_surface_ack();
        drop(client);
        quit.store(true, std::sync::atomic::Ordering::Release);
        handle.join().unwrap().unwrap();
        let _ = std::fs::remove_file(path);
        (acks, flow)
    }

    #[cfg(unix)]
    #[test]
    fn remote_endpoint_negotiates_surface_ack_with_a_current_server() {
        assert_eq!(negotiate_surface_ack(false), (true, Some((0, 0))));
    }

    #[cfg(unix)]
    #[test]
    fn local_endpoint_does_not_negotiate_surface_ack() {
        assert_eq!(negotiate_surface_ack(true), (false, None));
    }

    #[cfg(unix)]
    #[test]
    fn remote_endpoint_sends_no_acks_to_a_server_without_the_capability() {
        let (mut client, mut server, path) = stream_pair("surface-ack-old-server");
        let old_server = std::thread::spawn(move || {
            let hello: ClientMessage = protocol::read_message(&mut server, MAX_FRAME_SIZE).unwrap();
            let ClientMessage::EndpointControl { data, .. } = hello else {
                panic!("expected endpoint hello");
            };
            let hello: EndpointClientHello = serde_json::from_str(&data).unwrap();
            assert!(hello.surface_ack);
            let mut welcome = EndpointServerWelcome::compatible(Vec::new());
            welcome
                .capabilities
                .retain(|capability| capability != SURFACE_ACK_CAPABILITY);
            protocol::write_message(
                &mut server,
                &ServerMessage::EndpointControl {
                    kind: ENDPOINT_WELCOME_KIND.into(),
                    data: serde_json::to_string(&welcome).unwrap(),
                },
            )
            .unwrap();
            server
        });
        let handshake = do_handshake(
            &mut client,
            80,
            24,
            8,
            16,
            false,
            Some(crate::protocol::ClientSurfaceSize { cols: 80, rows: 24 }),
            false,
            false,
            true,
            false,
        )
        .unwrap();
        drop(old_server.join().unwrap());
        assert!(!super::super::endpoint::EndpointNegotiation::new(
            Vec::new(),
            handshake.endpoint_capabilities.unwrap_or_default(),
        )
        .supports_surface_ack());
        let _ = std::fs::remove_file(path);
    }
}
