use super::*;

pub(super) fn start_endpoint_transport(
    stream: LocalStream,
    lifetime: impl Send + 'static,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    endpoint_id: endpoint::ClientEndpointId,
    generation: u64,
    max_frame_size: usize,
    surface_decoder: Option<protocol::surface_reuse::Decoder>,
    surface_ack: bool,
) -> Result<endpoint::NativeEndpointTransport, ClientError> {
    let reader = stream.try_clone().map_err(ClientError::ConnectionFailed)?;
    let transport = endpoint::NativeEndpointTransport::with_lifetime(stream, lifetime)
        .map_err(ClientError::ConnectionFailed)?;
    let stopped = transport.stop_handle();
    let surface_ack = surface_ack.then(|| transport.surface_ack());
    let event_tx = event_tx.clone();
    std::thread::Builder::new()
        .name("endpoint-reader".into())
        .spawn(move || {
            server_reader_thread(
                reader,
                event_tx,
                &stopped,
                max_frame_size,
                endpoint_id,
                generation,
                surface_decoder,
                surface_ack,
            );
        })
        .map_err(ClientError::ConnectionFailed)?;
    Ok(transport)
}

/// Unacknowledged received bytes after which the reader acks even without a surface frame, so a
/// run of other messages cannot hold the server's flow window closed.
const SURFACE_ACK_INTERVAL: u64 = 16 * 1024;

/// Reads complete frames while retaining partial-read progress across nonblocking polls.
/// With `surface_ack`, it acknowledges received bytes on receipt, because a background surface
/// is never presented but must still keep the server's flow window open.
pub(super) fn server_reader_thread(
    mut stream: LocalStream,
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    max_frame_size: usize,
    endpoint_id: endpoint::ClientEndpointId,
    generation: u64,
    mut surface_decoder: Option<protocol::surface_reuse::Decoder>,
    surface_ack: Option<endpoint::SurfaceAck>,
) {
    if stream.set_nonblocking(true).is_err() {
        let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected {
            endpoint_id,
            generation,
        });
        return;
    }

    let mut stream = EndpointReader {
        stream: &mut stream,
        stopped: should_quit,
        received: 0,
    };
    let mut acked = 0;
    loop {
        if should_quit.load(Ordering::Acquire) {
            break;
        }

        let message = protocol::read_message(&mut stream, max_frame_size).and_then(|message| {
            match &mut surface_decoder {
                Some(decoder) => decoder.decode_frame(message).map_err(|error| {
                    protocol::FramingError::Io(io::Error::new(io::ErrorKind::InvalidData, error))
                }),
                None => Ok(Some(message)),
            }
        });
        let surface_frame = matches!(
            &message,
            Ok(Some(
                ServerMessage::PaneSurface(_) | ServerMessage::PaneSurfacePatch(_)
            ))
        );
        match message {
            Ok(None) => {}
            Ok(Some(msg)) => {
                if event_tx
                    .blocking_send(ClientLoopEvent::ServerMessage {
                        endpoint_id: endpoint_id.clone(),
                        generation,
                        message: Box::new(msg),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Err(protocol::FramingError::Io(err)) if err.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(protocol::FramingError::UnexpectedEof) => {
                let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected {
                    endpoint_id: endpoint_id.clone(),
                    generation,
                });
                break;
            }
            Err(err) => {
                warn!(err = %err, "server read error");
                let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected {
                    endpoint_id: endpoint_id.clone(),
                    generation,
                });
                break;
            }
        }
        if let Some(ack) = &surface_ack {
            if surface_frame || stream.received - acked >= SURFACE_ACK_INTERVAL {
                ack.record(stream.received);
                acked = stream.received;
            }
        }
    }
}

struct EndpointReader<'a> {
    stream: &'a mut LocalStream,
    stopped: &'a AtomicBool,
    /// Every byte read since the handshake, matching the server's framed-byte count.
    received: u64,
}

impl io::Read for EndpointReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return Ok(0);
            }
            match crate::ipc::poll_local_stream_read_count(self.stream, buffer)? {
                crate::ipc::LocalStreamReadCount::Data(count) => {
                    self.received += count as u64;
                    return Ok(count);
                }
                crate::ipc::LocalStreamReadCount::Closed => return Ok(0),
                crate::ipc::LocalStreamReadCount::Pending => {
                    crate::platform::wait_client_stream_readable(self.stream)?;
                }
            }
        }
    }
}

pub(in crate::client) fn write_to_local_server(
    stream: &mut LocalStream,
    msg: &ClientMessage,
) -> io::Result<()> {
    protocol::write_message(stream, msg).map_err(|error| io::Error::other(error.to_string()))
}

pub(super) trait ClientMessageSink {
    fn send_client_message(&mut self, message: &ClientMessage) -> io::Result<()>;
}

impl ClientMessageSink for LocalStream {
    fn send_client_message(&mut self, message: &ClientMessage) -> io::Result<()> {
        write_to_local_server(self, message)
    }
}

impl ClientMessageSink for endpoint::EndpointRegistry {
    fn send_client_message(&mut self, message: &ClientMessage) -> io::Result<()> {
        // The lifecycle loop consumes failures for every endpoint, including Local. A send
        // failure must not bypass that transition or tear down unrelated connections.
        self.send(message);
        Ok(())
    }
}

pub(super) fn write_to_server(
    stream: &mut impl ClientMessageSink,
    msg: &ClientMessage,
) -> io::Result<()> {
    stream.send_client_message(msg)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::client::endpoint::EndpointTransport as _;
    use interprocess::local_socket::traits::Listener as _;
    use std::io::{Read as _, Write as _};
    use std::time::Instant;

    #[test]
    fn upload_cancellation_preserves_pending_endpoint_download() {
        let path = std::env::temp_dir().join(format!(
            "herdr-cancel-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let listener = crate::ipc::bind_private_local_listener(&path).unwrap();
        let client = crate::ipc::connect_local_stream(&path).unwrap();
        let mut bridge = listener.accept().unwrap();
        std::fs::remove_file(path).unwrap();
        drop(listener);
        let mut reader_stream = client.try_clone().unwrap();
        let mut writer = endpoint::NativeEndpointTransport::with_lifetime(client, ()).unwrap();
        let stopped = writer.stop_handle();
        struct ForwardedInput(std::sync::mpsc::Sender<Vec<u8>>);
        impl io::Write for ForwardedInput {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.send(bytes.to_vec()).unwrap();
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (forwarded_tx, forwarded_rx) = std::sync::mpsc::channel();
        let cancel = crate::remote::bridge_upload_cancellation_for_test(
            bridge.try_clone().unwrap(),
            ForwardedInput(forwarded_tx),
        );
        let message = ClientMessage::ClientShellFocus { focused: false };
        let mut expected = Vec::new();
        protocol::write_message(&mut expected, &message).unwrap();
        writer.send(&message).unwrap();
        let mut forwarded = Vec::new();
        while forwarded.len() < expected.len() {
            forwarded.extend(forwarded_rx.recv_timeout(Duration::from_secs(3)).unwrap());
        }
        assert_eq!(forwarded, expected);
        cancel();

        // A client write after upload cancellation must not stop the download reader.
        writer
            .send(&ClientMessage::ClientShellFocus { focused: true })
            .unwrap();
        let flushed = writer.flush(Instant::now() + Duration::from_secs(3));
        if flushed.is_ok() {
            let received: ClientMessage =
                protocol::read_message(&mut bridge, protocol::MAX_FRAME_SIZE).unwrap();
            assert_eq!(received, ClientMessage::ClientShellFocus { focused: true });
        }
        const FINAL: &[u8] = b"pending-download: FINAL OUTPUT\n";
        bridge.write_all(FINAL).unwrap();
        drop(bridge);
        let mut output = Vec::new();
        EndpointReader {
            stream: &mut reader_stream,
            stopped: &stopped,
            received: 0,
        }
        .read_to_end(&mut output)
        .unwrap();
        assert_eq!(output, FINAL);
        assert!(flushed.is_ok(), "client write failed: {flushed:?}");
        assert!(!stopped.load(Ordering::Acquire));
        assert!(writer.take_error().is_none());
    }

    #[test]
    fn surface_acks_count_the_same_bytes_the_server_sent() {
        use crate::server::client_transport::{handle_client_handshake, ServerEvent};
        let path = std::path::PathBuf::from("/tmp").join(format!(
            "ha{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let listener = crate::ipc::bind_private_local_listener(&path).unwrap();
        let mut client = crate::ipc::connect_local_stream(&path).unwrap();
        let server = listener.accept().unwrap();
        std::fs::remove_file(&path).unwrap();
        drop(listener);
        let (server_events, mut server_event_rx) = tokio::sync::mpsc::channel(64);
        let quit = Arc::new(AtomicBool::new(false));
        let server_quit = quit.clone();
        let server_thread = std::thread::spawn(move || {
            handle_client_handshake(server, 1, &server_events, &server_quit)
        });
        let handshake = super::super::do_handshake(
            &mut client,
            80,
            24,
            8,
            16,
            false,
            Some(protocol::ClientSurfaceSize { cols: 80, rows: 24 }),
            false,
            false,
            true,
            false,
        )
        .unwrap();
        let negotiation = endpoint::EndpointNegotiation::new(
            handshake.endpoint_methods.unwrap_or_default(),
            handshake.endpoint_capabilities.unwrap_or_default(),
        );
        let writer = match server_event_rx.blocking_recv().unwrap() {
            ServerEvent::ClientShellConnected { writer, .. } => writer,
            other => panic!("expected ClientShellConnected, got {other:?}"),
        };
        let (client_events, mut client_event_rx) = tokio::sync::mpsc::channel(1024);
        let transport = start_endpoint_transport(
            client,
            (),
            &client_events,
            endpoint::ClientEndpointId::Local,
            1,
            protocol::MAX_GRAPHICS_FRAME_SIZE,
            None,
            negotiation.supports_surface_ack(),
        )
        .unwrap();

        let frame = |message: &ServerMessage| {
            let mut bytes = Vec::new();
            protocol::write_message(&mut bytes, message).unwrap();
            bytes
        };
        let title = |len: usize| ServerMessage::WindowTitle {
            title: Some("t".repeat(len)),
        };
        let mut sent = 0;
        let control = frame(&ServerMessage::ReloadSoundConfig);
        sent += control.len();
        writer.control.send(control).unwrap();
        // A render may carry several frames in one write.
        let mut bundle = frame(&title(300));
        bundle.extend(frame(&ServerMessage::ClientShellKeyboardReportAll {
            enabled: true,
        }));
        // The last frame pushes the unacked tail past the ack interval, so it is acked.
        for render in [frame(&title(5)), bundle, frame(&title(20 * 1024))] {
            sent += render.len();
            let mut render = render;
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match writer.render.try_send(render) {
                    Ok(()) => break,
                    Err(std::sync::mpsc::TrySendError::Full(back)) => {
                        assert!(Instant::now() < deadline, "render slot never drained");
                        render = back;
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("render send failed: {error:?}"),
                }
            }
        }

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            while client_event_rx.try_recv().is_ok() {}
            let (server_sent, acked) = writer.test_flow_counts().unwrap();
            assert_eq!(server_sent, sent as u64);
            if acked == server_sent {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "client acked {acked} of {server_sent} bytes"
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        drop(writer);
        drop(transport);
        drop(client_events);
        quit.store(true, Ordering::Release);
        while client_event_rx.blocking_recv().is_some() {}
        server_thread.join().unwrap().unwrap();
    }
}
