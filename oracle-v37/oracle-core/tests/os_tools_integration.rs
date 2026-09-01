//! End-to-end test of the OS-control tools over a REAL actd socket.
//!
//! Spins up the actd server (mock platform) on a filesystem socket, connects
//! the same `ActdClient` the agent uses, wires it into `Shared`, and drives the
//! `os.list_windows` / `os.kill_process` tools through the tool dispatch path —
//! proving the agent can actually reach the daemon and that the safety gates
//! (irreversible → needs confirmation) hold across the wire.

#![cfg(unix)]

use std::sync::Arc;

use oracle_actd::audit::AuditJournal;
use oracle_actd::daemon::Daemon;
use oracle_actd::pal::MockPlatform;
use oracle_actd::server;
use oracle_core::confirm::Confirmer;
use oracle_core::connectors::actd_client::ActdClient;
use oracle_core::tools::{ToolCtx, ToolOutcome};
use oracle_core::Shared;
use oracle_ipc::actd::Capability;
use oracle_ipc::transport::unix;
use tokio::sync::watch;

/// A test confirmer that always sanctions — stands in for a user clicking
/// "Sanction" in the Apollo modal.
struct AlwaysAllow;
#[async_trait::async_trait]
impl Confirmer for AlwaysAllow {
    async fn request(&self, _prompt: &str, _severity: &str) -> bool {
        true
    }
}

/// A socket path short enough for `sockaddr_un.sun_path` on every unix.
///
/// `std::env::temp_dir()` is `/tmp/` on Linux but the ~49-byte `$TMPDIR` on
/// macOS, so the old path (temp_dir + a full UUID + "/actd.sock") was 108 bytes
/// and every test here failed to connect on a Mac. `scratch_socket_dir` is the
/// shared answer, next to the limit it respects.
fn temp_socket() -> (std::path::PathBuf, String) {
    let dir = unix::scratch_socket_dir("oracle-os-it").unwrap();
    let path = dir.join("actd.sock");
    (dir.clone(), path.to_str().unwrap().to_string())
}

/// Boot an actd server on a socket; returns (shutdown, join, dir, sock).
async fn boot_actd(
    grant_sensitive: bool,
) -> (
    watch::Sender<bool>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    std::path::PathBuf,
    String,
) {
    let (dir, sock) = temp_socket();
    let daemon = {
        let d = Daemon::new(
            MockPlatform::new(),
            AuditJournal::new(Box::new(std::io::sink())),
        );
        if grant_sensitive {
            d.policy_mut().grant(Capability::Sensitive);
        }
        Arc::new(d)
    };
    let (tx, rx) = watch::channel(false);
    let sock_for_server = sock.clone();
    let handle = tokio::spawn(async move { server::serve(&sock_for_server, daemon, rx).await });
    // Wait for the socket to appear.
    for _ in 0..100 {
        if std::path::Path::new(&sock).exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    (tx, handle, dir, sock)
}

async fn shared_with_actd(sock: &str) -> Arc<Shared> {
    let client = ActdClient::connect(sock).await.expect("connect actd");
    Arc::new(Shared::for_test().with_actd(Some(client)))
}

async fn shared_with_actd_and_approval(sock: &str) -> Arc<Shared> {
    let client = ActdClient::connect(sock).await.expect("connect actd");
    Arc::new(
        Shared::for_test()
            .with_actd(Some(client))
            .with_confirmer(Arc::new(AlwaysAllow)),
    )
}

/// Run a registered tool by name through the full dispatch (parse + validate + run).
async fn run_tool(shared: &Arc<Shared>, name: &str, args: serde_json::Value) -> ToolOutcome {
    let reg = oracle_core::demo_registry();
    let tool = reg.get(name).expect("tool registered");
    let ctx = ToolCtx {
        turn_id: uuid::Uuid::new_v4(),
        shared: shared.clone(),
    };
    tool.dispatch(args, &ctx).await
}

#[tokio::test]
async fn os_list_windows_reaches_the_daemon() {
    let (tx, handle, dir, sock) = boot_actd(false).await;
    let shared = shared_with_actd(&sock).await;

    let out = run_tool(&shared, "os.list_windows", serde_json::json!({})).await;
    match out {
        ToolOutcome::Ok(v) => {
            let windows = v["windows"].as_array().expect("windows array");
            assert!(!windows.is_empty(), "mock platform exposes windows");
            assert!(windows.iter().any(|w| w["title"] == "Terminal"));
        }
        ToolOutcome::Err(e) => panic!("expected windows, got error: {e:?}"),
    }

    let _ = tx.send(true);
    let _ = tokio::net::UnixStream::connect(&sock).await; // nudge accept loop
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn os_kill_process_declined_by_default_confirmer() {
    // The default confirmer (DenyConfirmer) refuses, so the parked kill is
    // discarded — the process is NOT terminated. Proves the safety gate holds:
    // no human sanction, no irreversible action.
    let (tx, handle, dir, sock) = boot_actd(true).await;
    let shared = shared_with_actd(&sock).await; // DenyConfirmer

    let out = run_tool(
        &shared,
        "os.kill_process",
        serde_json::json!({ "pid": 1002 }),
    )
    .await;
    match out {
        ToolOutcome::Err(e) => assert!(
            e.reason.to_lowercase().contains("declin"),
            "kill should be declined, got: {e:?}"
        ),
        ToolOutcome::Ok(v) => panic!("kill must NOT execute without sanction, got {v}"),
    }

    let _ = tx.send(true);
    let _ = tokio::net::UnixStream::connect(&sock).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn os_kill_process_executes_when_sanctioned() {
    // With an approving confirmer (user clicked "Sanction"), the tool sends the
    // Confirm RPC and the daemon actually terminates the process.
    let (tx, handle, dir, sock) = boot_actd(true).await;
    let shared = shared_with_actd_and_approval(&sock).await;

    let out = run_tool(
        &shared,
        "os.kill_process",
        serde_json::json!({ "pid": 1002 }),
    )
    .await;
    match out {
        ToolOutcome::Ok(v) => assert_eq!(v["ok"], true, "kill should execute after sanction"),
        ToolOutcome::Err(e) => panic!("sanctioned kill should execute, got error: {e:?}"),
    }

    // And the process is gone from a subsequent listing.
    let procs = run_tool(&shared, "os.list_processes", serde_json::json!({})).await;
    match procs {
        ToolOutcome::Ok(v) => {
            let list = v["processes"].as_array().unwrap();
            assert!(
                list.iter().all(|p| p["pid"] != 1002),
                "pid 1002 should be gone"
            );
        }
        ToolOutcome::Err(e) => panic!("list failed: {e:?}"),
    }

    let _ = tx.send(true);
    let _ = tokio::net::UnixStream::connect(&sock).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn os_tools_report_clearly_when_daemon_absent() {
    // No actd connected → the tool returns a clear "not connected" error.
    let shared = Arc::new(Shared::for_test()); // actd = None
    let out = run_tool(&shared, "os.list_processes", serde_json::json!({})).await;
    match out {
        ToolOutcome::Err(e) => assert!(e.reason.contains("not connected")),
        ToolOutcome::Ok(_) => panic!("should error without a daemon"),
    }
}
