use std::io;
use std::os::fd::{AsFd as _, AsRawFd, RawFd};
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};

use interprocess::local_socket::traits::{Listener as _, Stream as _};
use interprocess::local_socket::ListenerNonblockingMode;
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::ipc::LocalListener;
use crate::server::client_transport::{self, ServerEvent};

/// The nonblocking thin-client listener, plus its registration with the tokio
/// reactor so an idle server loop wakes as soon as a client connects.
pub(crate) struct ClientListener {
    // Declared before `listener` so the reactor registration is removed before
    // the listener closes its fd.
    wake: ListenerWake,
    listener: LocalListener,
}

enum ListenerWake {
    Unregistered,
    Registered(AsyncFd<ListenerFd>),
    /// Registration failed. The loop's periodic housekeeping wake still
    /// accepts, so the server keeps running with slower attaches.
    Unavailable,
}

impl ListenerWake {
    fn from_registration(registration: io::Result<AsyncFd<ListenerFd>>) -> Self {
        match registration {
            Ok(readiness) => Self::Registered(readiness),
            Err(err) => {
                warn!(
                    err = %err,
                    "client listener wake unavailable; new clients wait for the idle housekeeping wake"
                );
                Self::Unavailable
            }
        }
    }
}

/// The listener fd without ownership: `ClientListener` owns the listener.
struct ListenerFd(RawFd);

impl AsRawFd for ListenerFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl ClientListener {
    pub(crate) fn new(listener: LocalListener) -> io::Result<Self> {
        listener.set_nonblocking(ListenerNonblockingMode::Accept)?;
        Ok(Self {
            wake: ListenerWake::Unregistered,
            listener,
        })
    }

    pub(crate) fn listener(&self) -> &LocalListener {
        &self.listener
    }

    /// Resolves once a connection may be pending. The caller must then accept
    /// until `WouldBlock`. Readiness is cleared here, before that drain, so a
    /// client that arrives during the drain raises readiness again.
    ///
    /// The reactor registration happens on first use because tests build the
    /// server outside a tokio runtime. If it fails, this pends forever and is
    /// not retried. The only error returned means the reactor shut down.
    pub(crate) async fn wait_for_connection(&mut self) -> io::Result<()> {
        if matches!(self.wake, ListenerWake::Unregistered) {
            let LocalListener::UdSocket(listener) = &self.listener;
            self.wake = ListenerWake::from_registration(AsyncFd::with_interest(
                ListenerFd(listener.as_fd().as_raw_fd()),
                Interest::READABLE,
            ));
        }
        match &self.wake {
            ListenerWake::Registered(readiness) => {
                readiness.readable().await?.clear_ready();
                Ok(())
            }
            ListenerWake::Unavailable => std::future::pending().await,
            ListenerWake::Unregistered => unreachable!("registration was just attempted"),
        }
    }
}

/// Accepts pending thin-client connections and starts their handshake readers.
pub(crate) fn accept_pending_client_connections(
    listener: &LocalListener,
    next_client_id: &mut u64,
    should_quit: &Arc<AtomicBool>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) -> io::Result<()> {
    loop {
        if should_quit.load(Ordering::Acquire) {
            break;
        }
        match listener.accept() {
            Ok(stream) => {
                let client_id = *next_client_id;
                *next_client_id = next_client_id.saturating_add(1);

                if let Err(err) = stream.set_nonblocking(true) {
                    warn!(err = %err, "failed to set client stream nonblocking");
                    continue;
                }

                let should_quit = should_quit.clone();
                let server_event_tx = server_event_tx.clone();
                std::thread::spawn(move || {
                    if let Err(err) = client_transport::handle_client_handshake(
                        stream,
                        client_id,
                        &server_event_tx,
                        &should_quit,
                    ) {
                        debug!(client_id, err = %err, "client handshake failed");
                    }
                });
            }
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => {
                error!(err = %err, "client listener accept failed");
                break;
            }
        }
    }

    Ok(())
}

/// Drains pending thin-client connections without starting handshakes.
///
/// During live handoff the old server must not let clients sit in the Unix
/// listener backlog waiting for a welcome frame that will never be sent.
pub(crate) fn reject_pending_client_connections(listener: &LocalListener) -> io::Result<()> {
    loop {
        match listener.accept() {
            Ok(_stream) => {}
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => {
                error!(err = %err, "client listener reject failed");
                break;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use super::*;

    fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    fn accept_all(listener: &ClientListener) -> usize {
        let mut accepted = 0;
        loop {
            match listener.listener().accept() {
                Ok(_stream) => accepted += 1,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return accepted,
                Err(err) => panic!("accept failed: {err}"),
            }
        }
    }

    fn test_socket_path(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("hca-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("client.sock");
        let _ = std::fs::remove_file(&path);
        (dir, path)
    }

    // The timeouts only guard against a hang: readiness, not a timer, must
    // complete each wait.
    #[tokio::test]
    async fn connection_wakes_listener_wait_and_drain_rearms_it() {
        let (dir, path) = test_socket_path("wake");
        let mut listener =
            ClientListener::new(crate::ipc::bind_local_listener(&path).unwrap()).unwrap();

        for _ in 0..2 {
            let _client = {
                let wait = listener.wait_for_connection();
                tokio::pin!(wait);
                assert!(poll_once(wait.as_mut()).is_pending(), "idle listener woke");

                let client = crate::ipc::connect_local_stream(&path).unwrap();
                tokio::time::timeout(Duration::from_secs(5), wait)
                    .await
                    .expect("a new connection must wake the listener wait")
                    .unwrap();
                client
            };
            assert_eq!(accept_all(&listener), 1);
        }

        {
            let wait = listener.wait_for_connection();
            tokio::pin!(wait);
            assert!(
                poll_once(wait.as_mut()).is_pending(),
                "a drained listener must not stay ready"
            );
        }
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failed_registration_pends_without_retrying_and_accept_still_works() {
        let (dir, path) = test_socket_path("unavailable");
        let mut listener =
            ClientListener::new(crate::ipc::bind_local_listener(&path).unwrap()).unwrap();
        listener.wake =
            ListenerWake::from_registration(Err(io::Error::from_raw_os_error(libc::ENOSPC)));

        let _client = crate::ipc::connect_local_stream(&path).unwrap();
        for _ in 0..2 {
            let wait = listener.wait_for_connection();
            tokio::pin!(wait);
            assert!(
                poll_once(wait.as_mut()).is_pending(),
                "an unavailable wake must pend even with a client waiting"
            );
        }
        assert!(matches!(listener.wake, ListenerWake::Unavailable));
        assert_eq!(accept_all(&listener), 1, "the periodic accept still drains");

        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
