//! Length-framed message transport for the local process mesh (architecture
//! §0.5). Every internal RPC link (core↔actd) speaks this: a 4-byte big-endian
//! length prefix followed by a JSON-encoded payload. JSON keeps the reference
//! build debuggable; swapping to Protobuf/bincode is a change to `encode`/
//! `decode` alone, since the framing is orthogonal.
//!
//! Transport is Unix domain sockets on unix and named pipes on windows. The
//! server verifies the peer's credentials (`SO_PEERCRED`) so only the same-user
//! core process can drive the privileged daemon.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum single-frame size (16 MiB). A larger declared length is rejected
/// rather than allocated — a cheap DoS guard on the local socket.
pub const MAX_FRAME: u32 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("frame too large: {0} bytes (max {MAX_FRAME})")]
    FrameTooLarge(u32),
    #[error("peer closed the connection")]
    Closed,
    #[error("encode/decode: {0}")]
    Codec(String),
}

/// Write one length-framed, JSON-encoded message.
pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(msg).map_err(|e| TransportError::Codec(e.to_string()))?;
    if body.len() as u64 > MAX_FRAME as u64 {
        return Err(TransportError::FrameTooLarge(body.len() as u32));
    }
    let len = (body.len() as u32).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one length-framed, JSON-decoded message. Returns `Closed` on clean EOF
/// at a frame boundary (the normal way a peer disconnects).
pub async fn read_msg<R, T>(r: &mut R) -> Result<T, TransportError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(TransportError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(TransportError::FrameTooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(|e| TransportError::Codec(e.to_string()))
}

#[cfg(unix)]
pub mod unix {
    //! Unix domain socket server/client with peer-credential checks.
    use super::*;
    use tokio::net::{UnixListener, UnixStream};

    /// Bind a UDS listener, removing any stale socket file first. The socket is
    /// created with 0600 perms via a umask-safe path (parent dir should be user
    /// private, e.g. `$XDG_RUNTIME_DIR/oracle/`).
    pub fn bind(path: &str) -> io::Result<UnixListener> {
        // Remove a stale socket from a previous crash.
        if std::path::Path::new(path).exists() {
            let _ = std::fs::remove_file(path);
        }
        let listener = UnixListener::bind(path)?;
        // Tighten perms: only the owning user may connect.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(listener)
    }

    /// Verify the connected peer is the same uid as us (SO_PEERCRED). Returns
    /// the peer uid on success.
    pub fn verify_peer(stream: &UnixStream) -> io::Result<u32> {
        let creds = stream.peer_cred()?;
        let me = current_uid();
        if creds.uid() != me {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("peer uid {} != {}", creds.uid(), me),
            ));
        }
        Ok(creds.uid())
    }

    pub async fn connect(path: &str) -> io::Result<UnixStream> {
        UnixStream::connect(path).await
    }

    fn current_uid() -> u32 {
        // Avoids a libc dependency: read the real uid from /proc/self/status.
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("Uid:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|u| u.parse().ok())
            })
            .unwrap_or(0)
    }
}

#[cfg(windows)]
pub mod windows {
    //! Windows named-pipe server/client, the equivalent of the UDS module.
    //!
    //! The pipe lives at `\\.\pipe\<name>`. `reject_remote_clients(true)` (the
    //! default, set explicitly here) blocks any connection that isn't local, so
    //! the daemon is never reachable off-box. The pipe's default security
    //! descriptor grants the creating user (and administrators); for a stricter
    //! same-user-only DACL, apply a custom SD at create time (documented in
    //! docs/WINDOWS.md).
    //!
    //! NOTE: this module is Windows-only and is compiled on the target machine;
    //! it is not built in the Linux CI sandbox.
    use std::io;
    use std::time::Duration;
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    /// ERROR_PIPE_BUSY — all instances are busy; retry after a short wait.
    const ERROR_PIPE_BUSY: i32 = 231;

    /// Normalize a short name (`oracle-actd`) to a full pipe path.
    pub fn full_pipe_name(name: &str) -> String {
        if name.starts_with(r"\\.\pipe\") {
            name.to_string()
        } else {
            // Allow callers to pass either a bare name or a full path.
            let bare = name.rsplit(['\\', '/']).next().unwrap_or(name);
            format!(r"\\.\pipe\{bare}")
        }
    }

    /// Create the FIRST server instance for a pipe. Must be created before any
    /// client connects, and `first_pipe_instance(true)` ensures no other
    /// process already owns this pipe name (anti-squatting).
    pub fn create_first(pipe: &str) -> io::Result<NamedPipeServer> {
        ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(pipe)
    }

    /// Create a subsequent server instance so a new client can connect while the
    /// previous instance is being serviced.
    pub fn create_next(pipe: &str) -> io::Result<NamedPipeServer> {
        ServerOptions::new()
            .reject_remote_clients(true)
            .create(pipe)
    }

    /// Connect a client to the pipe, retrying while all instances are busy.
    pub async fn connect(pipe: &str) -> io::Result<NamedPipeClient> {
        loop {
            match ClientOptions::new().open(pipe) {
                Ok(c) => return Ok(c),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Ping {
        n: u32,
        msg: String,
    }

    #[tokio::test]
    async fn frame_roundtrip_in_memory() {
        // Use a duplex pipe to exercise write_msg/read_msg without a socket.
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let ping = Ping {
            n: 7,
            msg: "hello".into(),
        };
        write_msg(&mut a, &ping).await.unwrap();
        let got: Ping = read_msg(&mut b).await.unwrap();
        assert_eq!(got, ping);
    }

    #[tokio::test]
    async fn clean_eof_is_closed() {
        let (a, mut b) = tokio::io::duplex(1024);
        drop(a); // close the writer
        let r: Result<Ping, _> = read_msg(&mut b).await;
        assert!(matches!(r, Err(TransportError::Closed)));
    }

    #[tokio::test]
    async fn oversized_length_prefix_is_rejected() {
        // Hand-craft a frame claiming a huge length.
        let (mut a, mut b) = tokio::io::duplex(1024);
        let huge = (MAX_FRAME + 1).to_be_bytes();
        a.write_all(&huge).await.unwrap();
        a.flush().await.unwrap();
        let r: Result<Ping, _> = read_msg(&mut b).await;
        assert!(matches!(r, Err(TransportError::FrameTooLarge(_))));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uds_client_server_roundtrip_with_peer_check() {
        use super::unix;
        let dir = std::env::temp_dir().join(format!("oracle-uds-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sock");
        let path_str = path.to_str().unwrap().to_string();

        let listener = unix::bind(&path_str).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _addr) = listener.accept().await.unwrap();
            // Same-process, same-uid: must verify.
            let uid = unix::verify_peer(&stream).unwrap();
            let ping: Ping = read_msg(&mut stream).await.unwrap();
            // echo back n+1
            write_msg(
                &mut stream,
                &Ping {
                    n: ping.n + 1,
                    msg: format!("uid={uid}"),
                },
            )
            .await
            .unwrap();
        });

        let mut client = unix::connect(&path_str).await.unwrap();
        write_msg(
            &mut client,
            &Ping {
                n: 41,
                msg: "hi".into(),
            },
        )
        .await
        .unwrap();
        let reply: Ping = read_msg(&mut client).await.unwrap();
        assert_eq!(reply.n, 42);
        assert!(reply.msg.starts_with("uid="));
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
