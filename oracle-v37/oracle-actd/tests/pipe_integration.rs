//! End-to-end integration test over a REAL Windows named pipe.
//!
//! The counterpart of `socket_integration.rs`, which drives the same
//! privileged-action path over a Unix domain socket. That test is
//! `#![cfg(unix)]`, so until this file existed the Windows transport —
//! `server::serve`'s named-pipe accept loop and `transport::windows` —
//! had no end-to-end coverage on any platform: it compiled, and nothing
//! ever proved a request survived the wire.
//!
//! Same three assertions as the unix test, so a divergence between the two
//! transports shows up as one of them failing rather than as a behaviour
//! difference nobody notices: observe, confirmation-required, replay-denied.

#![cfg(windows)]

use std::sync::Arc;
use std::time::Duration;

use oracle_actd::audit::AuditJournal;
use oracle_actd::daemon::Daemon;
use oracle_actd::pal::MockPlatform;
use oracle_actd::server;
use oracle_ipc::actd::{ActEnvelope, ActRequest, ActResponse, Capability};
use oracle_ipc::transport::{read_msg, windows as winpipe, write_msg};
use tokio::sync::watch;
use uuid::Uuid;

/// A pipe name unique to this run.
///
/// Unlike a socket file, a pipe name is a machine-global identifier and
/// `create_first` deliberately refuses a name another process already owns, so
/// a fixed name would make two concurrent runs (or a leftover actd) fail with
/// "Access is denied" rather than run in isolation.
fn temp_pipe() -> String {
    let unique = &Uuid::new_v4().simple().to_string()[..8];
    format!("oracle-it-{unique}")
}

#[tokio::test]
async fn full_privileged_path_over_real_named_pipe() {
    let name = temp_pipe();
    let pipe = winpipe::full_pipe_name(&name);

    // Server: real named pipe, real decision path, granted Sensitive so
    // confirmations (rather than flat denials) are exercised.
    let daemon = {
        let d = Daemon::new(
            MockPlatform::new(),
            AuditJournal::new(Box::new(std::io::sink())),
        );
        d.policy_mut().grant(Capability::Sensitive);
        Arc::new(d)
    };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let name_for_server = name.clone();
    let server = tokio::spawn({
        let daemon = daemon.clone();
        async move { server::serve(&name_for_server, daemon, shutdown_rx).await }
    });

    // Wait for the listener to create the first instance. `winpipe::connect`
    // retries ERROR_PIPE_BUSY but not ERROR_FILE_NOT_FOUND, which is what a
    // not-yet-created pipe returns, so the retry belongs here.
    let mut client = {
        let mut last = None;
        let mut connected = None;
        for _ in 0..100 {
            match winpipe::connect(&pipe).await {
                Ok(c) => {
                    connected = Some(c);
                    break;
                }
                Err(e) => {
                    last = Some(e);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
        connected.unwrap_or_else(|| panic!("actd never listened on {pipe}: {last:?}"))
    };

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

    // Clean shutdown. Dropping the client releases the serviced instance; the
    // accept loop is parked in `connect()` on the NEXT instance, so nudge it the
    // same way the unix test nudges its accept loop.
    drop(client);
    let _ = shutdown_tx.send(true);
    let _ = winpipe::connect(&pipe).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

/// The name normalizer accepts what callers actually pass: a bare name from a
/// config file, or an already-qualified path.
#[test]
fn a_pipe_name_normalizes_whether_or_not_it_is_qualified() {
    let bare = winpipe::full_pipe_name("oracle-actd");
    assert_eq!(bare, r"\\.\pipe\oracle-actd");
    // Already qualified: unchanged, not double-prefixed.
    assert_eq!(winpipe::full_pipe_name(&bare), bare);
}
