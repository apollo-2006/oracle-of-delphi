//! End-to-end test of the ambient index over a REAL actd socket.
//!
//! Boots actd on a filesystem socket with the mock platform, captures the
//! "focused window" through the same `ActdClient` the sampler uses, and drives
//! the frame through decode, change detection and storage. This is the seam the
//! unit tests cannot cover: the wire type surviving JSON, base64 round-tripping
//! a real PNG, and an observation actually landing in memory.
//!
//! What it does NOT cover is the vision model — there is no VLM in CI. The
//! interpretation step is exercised with a stub so the plumbing either side of
//! it is proven; the model call itself is the part that needs your hardware.

#![cfg(unix)]

use std::sync::Arc;

use oracle_actd::audit::AuditJournal;
use oracle_actd::daemon::Daemon;
use oracle_actd::pal::MockPlatform;
use oracle_actd::server;
use oracle_core::ambient::{frame, render_observation, FrameQueue, PendingFrame};
use oracle_core::connectors::actd_client::ActdClient;
use oracle_core::memory::EpisodeKind;
use oracle_core::Shared;
use oracle_ipc::actd::{ActRequest, ActResponse, CapturedImage};
use tokio::sync::watch;

fn temp_socket() -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("oracle-ambient-it-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("actd.sock");
    (dir.clone(), path.to_str().unwrap().to_string())
}

async fn boot_actd() -> (
    watch::Sender<bool>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    std::path::PathBuf,
    String,
) {
    let (dir, sock) = temp_socket();
    let daemon = Arc::new(Daemon::new(
        MockPlatform::new(),
        AuditJournal::new(Box::new(std::io::sink())),
    ));
    let (tx, rx) = watch::channel(false);
    let sock_for_server = sock.clone();
    let handle = tokio::spawn(async move { server::serve(&sock_for_server, daemon, rx).await });
    for _ in 0..100 {
        if std::path::Path::new(&sock).exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    (tx, handle, dir, sock)
}

async fn capture(client: &ActdClient<tokio::net::UnixStream>, max_width: u32) -> CapturedImage {
    let resp = client
        .call(
            uuid::Uuid::new_v4(),
            ActRequest::CaptureWindow {
                window_id: None,
                max_width: Some(max_width),
            },
        )
        .await
        .expect("capture call");
    match resp {
        ActResponse::Ok { data } => {
            serde_json::from_value(data.get("image").expect("image field").clone())
                .expect("CapturedImage decodes")
        }
        other => panic!("capture was not Ok: {other:?}"),
    }
}

/// Tear down in the order the existing socket tests use: signal, drop the
/// client (`serve` will not return while a connection is live), nudge the
/// accept loop so it notices, then bound the wait rather than hanging on it.
async fn shutdown(
    tx: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    dir: std::path::PathBuf,
    sock: &str,
    client: ActdClient<tokio::net::UnixStream>,
) {
    let _ = tx.send(true);
    drop(client);
    let _ = tokio::net::UnixStream::connect(sock).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    let _ = std::fs::remove_dir_all(dir);
}

fn decode(img: &CapturedImage) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(&img.png_b64)
        .expect("png_b64 is valid base64")
}

#[tokio::test]
async fn a_capture_survives_the_wire_as_a_decodable_png() {
    let (tx, handle, dir, sock) = boot_actd().await;
    let client = ActdClient::connect(&sock).await.expect("connect");

    let img = capture(&client, 256).await;
    assert!(
        !img.title.is_empty(),
        "the mock's focused window has a title"
    );
    let png = decode(&img);
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "PNG magic survived base64");
    // The header dimensions must agree with what the wire type claims, or the
    // caller's change detection is hashing something other than it thinks.
    assert_eq!(
        u32::from_be_bytes(png[16..20].try_into().unwrap()),
        img.width
    );
    assert_eq!(
        u32::from_be_bytes(png[20..24].try_into().unwrap()),
        img.height
    );
    assert!(frame::ahash_png(&png).is_some(), "the frame must hash");

    shutdown(tx, handle, dir, &sock, client).await;
}

#[tokio::test]
async fn an_unchanged_screen_is_not_reinterpreted() {
    // The whole economics of the index: capture is cheap and runs on a timer,
    // but most frames show the same thing as the last one. If this filter is
    // broken, every cycle costs a model call and writes a duplicate memory.
    let (tx, handle, dir, sock) = boot_actd().await;
    let client = ActdClient::connect(&sock).await.expect("connect");

    let a = frame::ahash_png(&decode(&capture(&client, 256).await)).unwrap();
    let b = frame::ahash_png(&decode(&capture(&client, 256).await)).unwrap();
    assert_eq!(a, b, "the mock screen did not change");
    assert!(!frame::is_new_scene(Some(a), b, 6));

    shutdown(tx, handle, dir, &sock, client).await;
}

#[tokio::test]
async fn a_different_window_reads_as_a_new_scene() {
    let (tx, handle, dir, sock) = boot_actd().await;
    let client = ActdClient::connect(&sock).await.expect("connect");

    let windows = match client
        .call(uuid::Uuid::new_v4(), ActRequest::ListWindows)
        .await
        .unwrap()
    {
        ActResponse::Ok { data } => data,
        other => panic!("{other:?}"),
    };
    let ids: Vec<u64> = windows["windows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_u64().unwrap())
        .collect();
    assert!(ids.len() >= 2, "need two windows");

    let mut hashes = Vec::new();
    for id in ids.iter().take(2) {
        let resp = client
            .call(
                uuid::Uuid::new_v4(),
                ActRequest::CaptureWindow {
                    window_id: Some(*id),
                    max_width: Some(256),
                },
            )
            .await
            .unwrap();
        let img: CapturedImage = match resp {
            ActResponse::Ok { data } => serde_json::from_value(data["image"].clone()).unwrap(),
            other => panic!("{other:?}"),
        };
        hashes.push(frame::ahash_png(&decode(&img)).unwrap());
    }
    assert!(
        frame::is_new_scene(Some(hashes[0]), hashes[1], 0),
        "two different windows must not collapse to one scene"
    );

    shutdown(tx, handle, dir, &sock, client).await;
}

#[tokio::test]
async fn an_interpreted_frame_becomes_a_searchable_memory() {
    // The payoff path: what the model saw is retrievable later by describing it.
    // The VLM itself is stubbed -- CI has no vision model -- so this proves the
    // storage and retrieval half, which is the half that can silently rot.
    let shared = Shared::for_test();
    let text = render_observation(
        "dispatch.rs — oracle",
        "Rust source for the tool dispatcher, showing a borrow checker error",
    );
    shared
        .memory
        .insert(EpisodeKind::Observation, &text, 0.25)
        .expect("store observation");

    let hits = shared
        .memory
        .retrieve("borrow checker error in the dispatcher", 5)
        .expect("retrieve");
    assert!(!hits.is_empty(), "the observation must be findable");
    assert!(hits[0].episode.text.contains("dispatch.rs"));
    assert_eq!(hits[0].episode.kind, EpisodeKind::Observation);
}

#[tokio::test]
async fn the_sampler_never_targets_pythias_own_window() {
    // The bug: the sampler asked for `window_id: None`, which resolves to the
    // FOREGROUND window -- and right after the user speaks, that is the HUD. The
    // index would store a description of Pythia's own UI as a memory of what the
    // user was looking at. The prompt's window block already avoided this; the
    // sampler was written without it.
    let (tx, handle, dir, sock) = boot_actd().await;
    let client = ActdClient::connect(&sock).await.expect("connect");

    let data = match client
        .call(uuid::Uuid::new_v4(), ActRequest::ListWindows)
        .await
        .unwrap()
    {
        ActResponse::Ok { data } => data,
        other => panic!("{other:?}"),
    };
    let mut windows = data["windows"].as_array().unwrap().clone();

    // Put our own window frontmost, exactly as it is after a voice turn.
    windows.insert(
        0,
        serde_json::json!({
            "id": 999_999_u64,
            "title": "Oracle of Delphi",
            "pid": 1,
            "focused": true,
            "minimized": false
        }),
    );

    let picked = oracle_core::screen::pick_target(&windows).expect("something behind us");
    assert_ne!(picked.0, 999_999, "must not target our own HUD");
    assert!(!picked.1.to_lowercase().contains("oracle of delphi"));

    // And the id it picked must actually be capturable.
    let resp = client
        .call(
            uuid::Uuid::new_v4(),
            ActRequest::CaptureWindow {
                window_id: Some(picked.0),
                max_width: Some(256),
            },
        )
        .await
        .unwrap();
    assert!(matches!(resp, ActResponse::Ok { .. }));

    shutdown(tx, handle, dir, &sock, client).await;
}

#[tokio::test]
async fn an_oversized_capture_is_resized_before_it_would_reach_the_model() {
    // macOS returns native size and ignores max_width -- on Retina, twice the
    // points asked for. The contract said the caller resizes; until this was
    // wired, nothing did, and the whole frame went to the vision model.
    let big = {
        let mut rgba = Vec::new();
        for y in 0..600u32 {
            for x in 0..2400u32 {
                rgba.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 128, 255]);
            }
        }
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 2400, 600);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(&rgba).unwrap();
        drop(wr);
        out
    };

    let resized = frame::fit_to_width(&big, 1024)
        .expect("decodes")
        .expect("must resize");
    assert!(
        resized.len() < big.len(),
        "resized {} bytes vs original {}",
        resized.len(),
        big.len()
    );
    assert_eq!(
        u32::from_be_bytes(resized[16..20].try_into().unwrap()),
        1024
    );
    assert!(frame::ahash_png(&resized).is_some());
}

#[tokio::test]
async fn frames_are_dropped_rather_than_queued_without_bound() {
    // Interpretation is slower than capture whenever the GPU is busy. The queue
    // must shed load instead of growing until the process dies -- and it must
    // shed the OLDEST, because the recent past is what is still worth indexing.
    let q = FrameQueue::new(3);
    for i in 0..10 {
        q.push(PendingFrame {
            captured_at: i,
            title: format!("w{i}"),
            png_b64: String::new(),
        });
    }
    assert_eq!(q.len(), 3);
    assert_eq!(q.dropped(), 7);
    assert_eq!(q.pop().unwrap().captured_at, 7, "kept the newest three");
}
