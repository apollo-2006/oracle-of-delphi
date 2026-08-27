//! End-to-end integration test over a REAL Unix domain socket.
//!
//! This spins up the actd server on a filesystem socket, connects the same
//! `ActdClient` that `oracle-core` uses, and drives the full privileged-action
//! path — observe, replay rejection, and the confirmation handshake — proving
//! the two-process security boundary works over the wire, not just in unit
//! tests.

#![cfg(unix)]

use std::sync::Arc;

use oracle_actd::audit::AuditJournal;
use oracle_actd::daemon::Daemon;
use oracle_actd::pal::MockPlatform;
use oracle_actd::server;
use oracle_ipc::actd::{ActEnvelope, ActRequest, ActResponse, Capability};
use oracle_ipc::transport::{read_msg, unix, write_msg};
use tokio::sync::watch;
use uuid::Uuid;

fn temp_socket() -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("oracle-it-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("actd.sock");
    let s = path.to_str().unwrap().to_string();
    (dir, s)
}

#[tokio::test]
async fn full_privileged_path_over_real_socket() {
    let (dir, sock) = temp_socket();

    // Server: real UDS, real decision path, granted Sensitive so confirmations
    // (rather than flat denials) are exercised.
    let daemon = {
        let d = Daemon::new(
            MockPlatform::new(),
            AuditJournal::new(Box::new(std::io::sink())),
        );
        d.policy_mut().grant(Capability::Sensitive);
        Arc::new(d)
    };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let sock_for_server = sock.clone();
    let server = tokio::spawn({
        let daemon = daemon.clone();
        async move { server::serve(&sock_for_server, daemon, shutdown_rx).await }
    });

    // Give the listener a moment to bind.
    for _ in 0..50 {
        if std::path::Path::new(&sock).exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Client connection (this is exactly what core uses).
    let mut client = unix::connect(&sock).await.expect("connect");
    let turn = Uuid::new_v4();

    // 1) Observe: list windows → Ok.
    write_msg(
        &mut client,
        &ActEnvelope {
            turn_id: turn,
            nonce: 1,
            request: ActRequest::ListWindows,
        },
    )
    .await
    .unwrap();
    let r: ActResponse = read_msg(&mut client).await.unwrap();
    match r {
        ActResponse::Ok { data } => {
            assert!(!data["windows"].as_array().unwrap().is_empty());
        }
        other => panic!("expected ok, got {other:?}"),
    }

    // 2) Kill: sensitive + irreversible → confirmation required over the wire.
    write_msg(
        &mut client,
        &ActEnvelope {
            turn_id: turn,
            nonce: 2,
            request: ActRequest::KillProcess { pid: 1002 },
        },
    )
    .await
    .unwrap();
    let r: ActResponse = read_msg(&mut client).await.unwrap();
    match r {
        ActResponse::Ok { data } => assert_eq!(data["needs_confirmation"], true),
        other => panic!("expected confirmation, got {other:?}"),
    }

    // 3) Replay the nonce → denied.
    write_msg(
        &mut client,
        &ActEnvelope {
            turn_id: turn,
            nonce: 2,
            request: ActRequest::ListProcesses,
        },
    )
    .await
    .unwrap();
    let r: ActResponse = read_msg(&mut client).await.unwrap();
    assert!(
        matches!(r, ActResponse::Denied { .. }),
        "replay must be denied"
    );

    // Clean shutdown.
    drop(client);
    let _ = shutdown_tx.send(true);
    // Nudge the accept loop by connecting once more so select wakes.
    let _ = unix::connect(&sock).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server).await;
    let _ = std::fs::remove_dir_all(&dir);
}
