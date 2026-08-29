//! The actd socket server (architecture §3.1): accepts framed [`ActEnvelope`]
//! RPCs from `oracle-core` over a Unix domain socket, verifies the peer uid,
//! runs each through the [`Daemon`] decision path, and writes back a framed
//! [`ActResponse`]. One connection is handled at a time by design — the daemon
//! is a serialization point for privileged actions, and its policy state is
//! inherently sequential.

use crate::daemon::Daemon;
use crate::pal::Platform;
use oracle_ipc::actd::{ActEnvelope, ActResponse};
use oracle_ipc::transport::{read_msg, write_msg, TransportError};
use std::sync::Arc;
use tokio::sync::watch;
use tracing::{info, warn};

/// Run the accept loop until `shutdown` flips to true. Returns when the socket
/// is closed and the final connection drained.
#[cfg(unix)]
pub async fn serve<P: Platform + 'static>(
    socket_path: &str,
    daemon: Arc<Daemon<P>>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    use oracle_ipc::transport::unix;
    let listener = unix::bind(socket_path)?;
    info!(socket = socket_path, "actd listening");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("actd shutdown requested");
                    break;
                }
            }
            accepted = listener.accept() => {
                let (mut stream, _addr) = match accepted {
                    Ok(s) => s,
                    Err(e) => { warn!("accept failed: {e}"); continue; }
                };
                // SO_PEERCRED: only the same-user core may drive us.
                match unix::verify_peer(&stream) {
                    Ok(uid) => info!(uid, "core connected"),
                    Err(e) => { warn!("rejected peer: {e}"); continue; }
                }
                let daemon = daemon.clone();
                // Handle this connection inline (serialized privileged actions).
                if let Err(e) = handle_conn(&mut stream, daemon).await {
                    match e {
                        TransportError::Closed => info!("core disconnected"),
                        other => warn!("connection error: {other}"),
                    }
                }
            }
        }
    }
    // Best-effort cleanup of the socket file.
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

/// Windows named-pipe server, the equivalent of the unix `serve`. Accepts one
/// client at a time on `\\.\pipe\<name>`; `reject_remote_clients` keeps it
/// local-only. This is compiled and run on the Windows target.
#[cfg(windows)]
pub async fn serve<P: Platform + 'static>(
    pipe_name: &str,
    daemon: Arc<Daemon<P>>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    use oracle_ipc::transport::windows as winpipe;
    let pipe = winpipe::full_pipe_name(pipe_name);
    // Bind the first pipe instance, retrying while a just-killed orphan releases
    // its handle. "Access is denied (os error 5)" here means another instance
    // still owns the name; core reaps orphans before launching us, so a short
    // retry rides out the brief window where the handle hasn't dropped yet.
    let mut server = {
        let mut attempt = 0u32;
        loop {
            match winpipe::create_first(&pipe) {
                Ok(s) => break s,
                Err(e) if e.raw_os_error() == Some(5) && attempt < 25 => {
                    if attempt == 0 {
                        warn!(pipe = %pipe, "pipe busy (access denied); waiting for a stale actd to release it");
                    }
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    };
    info!(pipe = %pipe, "actd listening (named pipe)");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("actd shutdown requested");
                    break;
                }
            }
            connected = server.connect() => {
                if let Err(e) = connected {
                    warn!("pipe connect failed: {e}");
                    continue;
                }
                info!("core connected");
                // Move the connected instance out and create the next one so a
                // subsequent client can connect while we service this one.
                let mut this = server;
                server = winpipe::create_next(&pipe)?;
                let daemon = daemon.clone();
                if let Err(e) = handle_conn(&mut this, daemon).await {
                    match e {
                        TransportError::Closed => info!("core disconnected"),
                        other => warn!("connection error: {other}"),
                    }
                }
            }
        }
    }
    Ok(())
}

/// Serve requests on one connection until the peer closes it. Split out so it
/// can be tested over any AsyncRead+AsyncWrite (including an in-memory duplex).
pub async fn handle_conn<P, S>(stream: &mut S, daemon: Arc<Daemon<P>>) -> Result<(), TransportError>
where
    P: Platform,
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let env: ActEnvelope = match read_msg(stream).await {
            Ok(e) => e,
            Err(TransportError::Closed) => return Ok(()),
            Err(e) => return Err(e),
        };
        // Confirmations arrive as a distinct op flow; here we handle the base
        // request path. (The confirm RPC is modeled in the integration test.)
        let resp: ActResponse = daemon.handle(env);
        write_msg(stream, &resp).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditJournal;
    use crate::pal::MockPlatform;
    use oracle_ipc::actd::{ActRequest, Capability};

    fn daemon() -> Arc<Daemon<MockPlatform>> {
        let d = Daemon::new(
            MockPlatform::new(),
            AuditJournal::new(Box::new(std::io::sink())),
        );
        d.policy_mut().grant(Capability::Sensitive);
        Arc::new(d)
    }

    #[tokio::test]
    async fn serves_requests_over_a_duplex_pipe() {
        // client <-> server over an in-memory duplex, exercising the real
        // framing + decision path without needing a filesystem socket.
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let d = daemon();
        let srv = tokio::spawn(async move { handle_conn(&mut server, d).await });

        // Request 1: list windows (observe → ok).
        write_msg(
            &mut client,
            &ActEnvelope {
                turn_id: uuid::Uuid::new_v4(),
                nonce: 1,
                request: ActRequest::ListWindows,
            },
        )
        .await
        .unwrap();
        let r1: ActResponse = read_msg(&mut client).await.unwrap();
        assert!(matches!(r1, ActResponse::Ok { .. }));

        // Request 2: replayed nonce → denied.
        write_msg(
            &mut client,
            &ActEnvelope {
                turn_id: uuid::Uuid::new_v4(),
                nonce: 1,
                request: ActRequest::ListProcesses,
            },
        )
        .await
        .unwrap();
        let r2: ActResponse = read_msg(&mut client).await.unwrap();
        assert!(matches!(r2, ActResponse::Denied { .. }));

        drop(client); // triggers clean shutdown of handle_conn
        srv.await.unwrap().unwrap();
    }
}
