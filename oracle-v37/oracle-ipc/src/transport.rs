//! Length-framed message transport for the local process mesh (architecture
//! §0.5). Every internal RPC link (core↔actd) speaks this: a 4-byte big-endian
//! length prefix followed by a JSON-encoded payload. JSON keeps the reference
//! build debuggable; swapping to Protobuf/bincode is a change to `encode`/
//! `decode` alone, since the framing is orthogonal.
//!
//! Transport is Unix domain sockets on unix and named pipes on windows. The
//! server verifies the peer's credentials so only the same-user core process
//! can drive the privileged daemon (`SO_PEERCRED` on Linux, `LOCAL_PEERCRED`
//! on macOS and the BSDs; tokio abstracts both behind `peer_cred`).

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

    /// The largest usable unix socket path, in bytes.
    ///
    /// The kernel copies the path into `sockaddr_un.sun_path`, a fixed char
    /// array: **104 bytes on macOS and the BSDs**, 108 on Linux. We use the
    /// smaller of the two everywhere, so a path that works on one platform
    /// works on all of them.
    ///
    /// This is not a soft limit and it does not truncate. Over it, `bind` and
    /// `connect` both fail with `InvalidInput: path must be shorter than
    /// SUN_LEN` — an error that names neither the path nor the limit, which is
    /// why [`check_path_len`] rewrites it below.
    pub const SUN_PATH_MAX: usize = 104;

    /// Reject an over-long socket path with an error that says what is wrong.
    ///
    /// macOS is where this matters. `std::env::temp_dir()` is `/tmp/` on Linux
    /// but `$TMPDIR` on macOS, which is a per-user hashed path such as
    /// `/var/folders/jj/cvft_wmn3cs4cl2pywmqvb3w0000gn/T/` — 49 bytes spent
    /// before the caller has contributed anything. A socket path assembled from
    /// it plus a UUID is comfortably legal on Linux and illegal on a Mac.
    pub fn check_path_len(path: &str) -> io::Result<()> {
        if path.len() >= SUN_PATH_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unix socket path is {} bytes; the limit is {SUN_PATH_MAX} \
                     (sun_path is 104 bytes on macOS/BSD, 108 on Linux). \
                     Use a shorter directory — /tmp/oracle always fits. Path: {path}",
                    path.len()
                ),
            ));
        }
        Ok(())
    }

    /// A short-enough private directory to put a socket in, created on demand.
    ///
    /// Deliberately rooted at `/tmp` rather than [`std::env::temp_dir`]: `/tmp`
    /// is four bytes on every unix, while `temp_dir()` on macOS is the ~49-byte
    /// `$TMPDIR` that pushes any socket beyond [`SUN_PATH_MAX`]. The directory
    /// is created 0700, so it is private to this user the same way
    /// `$XDG_RUNTIME_DIR` would be.
    ///
    /// The suffix is eight hex digits rather than a full 36-character UUID:
    /// still unique enough to keep concurrent runs apart, and 28 bytes cheaper
    /// against a budget that is already tight on macOS.
    pub fn scratch_socket_dir(prefix: &str) -> io::Result<std::path::PathBuf> {
        let unique = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let dir = std::path::PathBuf::from("/tmp").join(format!("{prefix}-{unique}"));
        std::fs::create_dir_all(&dir)?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(dir)
    }

    /// Bind a UDS listener, removing any stale socket file first. The socket is
    /// created with 0600 perms via a umask-safe path, inside a parent directory
    /// this function creates 0700 if it does not already exist.
    ///
    /// Creating the parent is not a convenience. `bind` on a path whose
    /// directory is missing fails with a bare `ENOENT` — "No such file or
    /// directory (os error 2)" — which names neither the socket nor the missing
    /// directory, and in actd propagates straight out of `main` so the daemon
    /// dies before it logs anything useful. Core then reports only "actd not
    /// connected", pointing at the wrong process entirely.
    ///
    /// This was invisible on Linux because the socket conventionally lives
    /// inside `$XDG_RUNTIME_DIR/oracle/`, which core already creates for its
    /// own logs. Put the socket anywhere else — as macOS must, since sun_path's
    /// 104-byte limit pushes it out of a long runtime dir — and nothing creates
    /// it. actd already does exactly this for its audit journal; the socket was
    /// simply never given the same treatment.
    pub fn bind(path: &str) -> io::Result<UnixListener> {
        check_path_len(path)?;
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!("creating socket directory {}: {e}", parent.display()),
                    )
                })?;
                // Private to this user, like $XDG_RUNTIME_DIR would be. Only on
                // a directory we just made: never widen or narrow one the user
                // already set up deliberately.
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        // Remove a stale socket from a previous crash.
        if std::path::Path::new(path).exists() {
            let _ = std::fs::remove_file(path);
        }
        let listener = UnixListener::bind(path)
            .map_err(|e| io::Error::new(e.kind(), format!("binding unix socket {path}: {e}")))?;
        // Tighten perms: only the owning user may connect. This applies to every
        // unix target, not just Linux -- macOS honours socket file permissions
        // the same way, and gating it on Linux left the socket world-accessible
        // there.
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(listener)
    }

    /// Verify the connected peer is the same uid as us. Returns the peer uid on
    /// success.
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
        check_path_len(path)?;
        UnixStream::connect(path).await
    }

    /// The real uid of this process.
    ///
    /// This used to read `/proc/self/status` unconditionally to avoid a libc
    /// dependency. There is no `/proc` on macOS or the BSDs, so the read failed
    /// and the `unwrap_or(0)` handed back uid 0 -- which never matches the real
    /// peer uid, so *every* connection was rejected and actd was unreachable on
    /// those platforms.
    ///
    /// `/proc` stays the fast path on Linux; everywhere else falls back to the
    /// `id -ur` shell-out, which is POSIX and needs no new dependency.
    fn current_uid() -> u32 {
        #[cfg(target_os = "linux")]
        {
            if let Some(uid) = proc_uid() {
                return uid;
            }
        }
        // Nothing left to try: fall back to a value that cannot match a real
        // peer, so the check fails closed rather than authenticating anyone.
        posix_uid().unwrap_or(u32::MAX)
    }

    #[cfg(target_os = "linux")]
    fn proc_uid() -> Option<u32> {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("Uid:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|u| u.parse().ok())
            })
    }

    /// Test hook: `current_uid` is private, but its correctness is the whole
    /// basis of peer authentication, so it needs direct coverage.
    #[cfg(test)]
    pub(crate) fn current_uid_for_test() -> u32 {
        current_uid()
    }

    fn posix_uid() -> Option<u32> {
        let out = std::process::Command::new("id").arg("-ur").output().ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout).ok()?.trim().parse().ok()
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
    #[test]
    fn an_over_long_socket_path_is_refused_with_a_useful_error() {
        use super::unix;
        let long = format!("/tmp/{}/actd.sock", "x".repeat(120));
        let err = unix::check_path_len(&long).expect_err("must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let msg = err.to_string();
        // The whole point is that the message says what the kernel's does not:
        // how long the path is, what the limit is, and which path it was.
        assert!(msg.contains("sun_path"), "{msg}");
        assert!(msg.contains(&long.len().to_string()), "{msg}");
        assert!(msg.contains(&long), "{msg}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_creates_a_missing_socket_directory() {
        use super::unix;
        // The exact shape that killed actd on macOS: a socket in a directory
        // that does not exist yet. Before this, bind returned a bare ENOENT and
        // the daemon exited before logging anything.
        let base = unix::scratch_socket_dir("oracle-mkdir-test").unwrap();
        let nested = base.join("run");
        assert!(!nested.exists(), "the directory must not exist yet");
        let sock = nested.join("actd.sock");
        let path = sock.to_str().unwrap();

        let listener = unix::bind(path).expect("bind must create the directory");
        assert!(nested.is_dir(), "bind should have created {nested:?}");
        assert!(sock.exists(), "the socket itself should exist");

        // Private to this user, like $XDG_RUNTIME_DIR.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "socket dir should be private, got {mode:o}");

        drop(listener);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn a_bind_error_names_the_socket_it_failed_on() {
        use super::unix;
        // "No such file or directory (os error 2)" with no path in it is what
        // made this take a debugging session rather than a glance at the log.
        let err = unix::bind("/proc/nonexistent-oracle-test/actd.sock")
            .or_else(|_| unix::bind("/dev/null/actd.sock"))
            .expect_err("binding under a non-directory must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("actd.sock"),
            "the error must name the socket path, got: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_scratch_socket_dir_leaves_room_for_a_socket_name() {
        use super::unix;
        let dir = unix::scratch_socket_dir("oracle-len-test").unwrap();
        let sock = dir.join("actd.sock");
        let path = sock.to_str().unwrap();
        unix::check_path_len(path).expect("a scratch dir must fit a socket path on every platform");
        // Real headroom, not a value that only just squeaks under the limit on
        // this machine: the caller may use a longer prefix than this test does.
        assert!(
            path.len() < unix::SUN_PATH_MAX / 2,
            "{} bytes leaves too little room: {path}",
            path.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uds_client_server_roundtrip_with_peer_check() {
        use super::unix;
        let dir = unix::scratch_socket_dir("oracle-uds").unwrap();
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

#[cfg(all(test, unix))]
mod uid_tests {
    /// The uid must resolve on every unix, not just Linux. A wrong answer here
    /// is not a degraded feature: verify_peer compares against it, so a bad uid
    /// rejects every connection and the daemon is simply unreachable.
    #[test]
    fn current_uid_matches_the_shell() {
        let listener_uid = super::unix::current_uid_for_test();
        let expected: u32 = String::from_utf8(
            std::process::Command::new("id")
                .arg("-ur")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .parse()
        .unwrap();
        assert_eq!(listener_uid, expected);
        assert_ne!(
            listener_uid,
            u32::MAX,
            "uid resolution fell through to fail-closed"
        );
    }
}
