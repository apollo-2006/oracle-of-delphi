//! Client for the privileged actuator daemon (architecture §3.1).
//!
//! `oracle-core` opens one long-lived connection to `oracle-actd` — a Unix
//! domain socket on unix, a named pipe on Windows — and drives it with framed
//! [`ActEnvelope`] RPCs. A monotonic nonce is maintained per connection for
//! replay protection, matching the daemon's high-water-mark check.
//!
//! The client is generic over the stream type so the request/response logic is
//! shared across platforms; only `connect` differs.

use oracle_ipc::actd::{ActEnvelope, ActRequest, ActResponse};
use oracle_ipc::transport::{read_msg, write_msg, TransportError};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use uuid::Uuid;

/// A connected actd client over a stream `S`.
pub struct ActdClient<S> {
    stream: Mutex<S>,
    nonce: AtomicU64,
}

impl<S: AsyncRead + AsyncWrite + Unpin> ActdClient<S> {
    /// Wrap an already-connected stream.
    pub fn from_stream(stream: S) -> Self {
        ActdClient {
            stream: Mutex::new(stream),
            nonce: AtomicU64::new(0),
        }
    }

    /// Issue one request within a turn. Requests are serialized over the single
    /// connection (the daemon handles them sequentially anyway).
    pub async fn call(
        &self,
        turn_id: Uuid,
        request: ActRequest,
    ) -> Result<ActResponse, TransportError> {
        let nonce = self.nonce.fetch_add(1, Ordering::SeqCst) + 1;
        let env = ActEnvelope {
            turn_id,
            nonce,
            request,
        };
        let mut stream = self.stream.lock().await;
        write_msg(&mut *stream, &env).await?;
        read_msg(&mut *stream).await
    }
}

#[cfg(unix)]
impl ActdClient<tokio::net::UnixStream> {
    /// Connect to the daemon's Unix domain socket.
    pub async fn connect(socket_path: &str) -> Result<Self, TransportError> {
        let stream = oracle_ipc::transport::unix::connect(socket_path)
            .await
            .map_err(TransportError::Io)?;
        Ok(Self::from_stream(stream))
    }
}

#[cfg(windows)]
impl ActdClient<tokio::net::windows::named_pipe::NamedPipeClient> {
    /// Connect to the daemon's named pipe (`\\.\pipe\<name>`).
    pub async fn connect(pipe_name: &str) -> Result<Self, TransportError> {
        let pipe = oracle_ipc::transport::windows::full_pipe_name(pipe_name);
        let stream = oracle_ipc::transport::windows::connect(&pipe)
            .await
            .map_err(TransportError::Io)?;
        Ok(Self::from_stream(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oracle_ipc::actd::ActRequest;

    /// The client's request/response logic works over any duplex stream — here
    /// an in-memory pipe with a trivial echo server, so it's platform-neutral.
    #[tokio::test]
    async fn client_call_roundtrips_over_duplex() {
        let (client_end, mut server_end) = tokio::io::duplex(64 * 1024);
        // Fake server: read one envelope, reply Ok.
        tokio::spawn(async move {
            let env: ActEnvelope = read_msg(&mut server_end).await.unwrap();
            assert_eq!(env.nonce, 1);
            let resp = ActResponse::Ok {
                data: serde_json::json!({ "echoed": env.nonce }),
            };
            write_msg(&mut server_end, &resp).await.unwrap();
        });

        let client = ActdClient::from_stream(client_end);
        let resp = client
            .call(Uuid::new_v4(), ActRequest::ListWindows)
            .await
            .unwrap();
        match resp {
            ActResponse::Ok { data } => assert_eq!(data["echoed"], 1),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
