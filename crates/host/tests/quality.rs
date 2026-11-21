//! Frames per second and picture quality changed during a session (the viewer's Quality
//! menu) reach the video loop right away.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rand::rngs::OsRng;
use rdesk_common::auth::{self, ClientAuth, KeySource};
use rdesk_common::config::{HostConfig, NetworkConfig, TransportTuning};
use rdesk_common::transport::{self, Role};
use rdesk_host::{HostHandle, HostState, Reporter, RunOpts};
use rdesk_proto::{
    ClientMsg, FrameAck, FrameHeader, MAX_VIDEO_MSG, StreamOpen, Tile, TileEncoding, expect_msg, read_msg, write_msg,
};
use ssh_key::{Algorithm, PrivateKey};

/// One frame as received: when, and the JPEG tiles it carried (count, bytes).
struct Seen {
    at: Instant,
    jpeg_tiles: usize,
    jpeg_bytes: usize,
}

/// Frames received in `window`, and the average size of their JPEG tiles.
fn measure(log: &Mutex<Vec<Seen>>, from: Instant, window: Duration) -> (usize, usize) {
    let log = log.lock().unwrap();
    let seen: Vec<&Seen> = log.iter().filter(|s| s.at >= from && s.at < from + window).collect();
    let tiles: usize = seen.iter().map(|s| s.jpeg_tiles).sum();
    let bytes: usize = seen.iter().map(|s| s.jpeg_bytes).sum();
    (seen.len(), bytes.checked_div(tiles).unwrap_or(0))
}

#[tokio::test(flavor = "multi_thread")]
async fn quality_changes_apply_during_the_session() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before any thread reads the environment.
    unsafe { std::env::set_var("RDESK_CONFIG_DIR", dir.path()) };
    let fp = rdesk_host::admin::ensure_initialized().unwrap();
    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    rdesk_host::admin::save_config(&HostConfig { port, max_fps: 30, jpeg_quality: 70, ..Default::default() }).unwrap();
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    rdesk_host::admin::add_key(key.public_key()).unwrap();
    let h = HostHandle::start(
        RunOpts { fake_capture: true, fake_input: true, ..Default::default() },
        &tokio::runtime::Handle::current(),
        Reporter::silent(),
    );
    for _ in 0..100 {
        if matches!(h.status().state, HostState::Listening(_)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let addr = format!("127.0.0.1:{port}");
    let net = NetworkConfig::default();
    let ep = transport::bind(None, &net, &TransportTuning::default(), Role::Client).await.unwrap();
    let c = transport::connect(&ep, fp, Some(&addr), &net, Duration::from_secs(5)).await.unwrap();
    let (mut w, mut r) = c.conn.open_bi().await.unwrap();
    let mut creds = ClientAuth {
        client_name: "laptop".into(),
        resume: None,
        ticket: None,
        keys: Some(KeySource::Local(vec![key])),
        key_filter: None,
        password: None,
    };
    auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut creds).await.unwrap();

    // Receive and acknowledge every frame, as the viewer does.
    let log = Arc::new(Mutex::new(Vec::<Seen>::new()));
    let mut acks = c.conn.open_uni().await.unwrap();
    write_msg(&mut acks, &StreamOpen::Acks).await.unwrap();
    let frames = {
        let (conn, log) = (c.conn.clone(), log.clone());
        tokio::spawn(async move {
            while let Ok(mut s) = conn.accept_uni().await {
                let header: FrameHeader = expect_msg(&mut s, MAX_VIDEO_MSG).await.unwrap();
                let (mut jpeg_tiles, mut jpeg_bytes) = (0, 0);
                while let Some(t) = read_msg::<_, Tile>(&mut s, MAX_VIDEO_MSG).await.unwrap() {
                    if t.enc == TileEncoding::Jpeg {
                        jpeg_tiles += 1;
                        jpeg_bytes += t.data.len();
                    }
                }
                log.lock().unwrap().push(Seen { at: Instant::now(), jpeg_tiles, jpeg_bytes });
                if write_msg(&mut acks, &FrameAck { frame_id: header.frame_id }).await.is_err() {
                    break;
                }
            }
        })
    };

    let window = Duration::from_secs(2);
    let settle = Duration::from_millis(500);
    // The host's own settings: 30 fps, quality 70.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let start = Instant::now();
    tokio::time::sleep(window).await;
    let (normal, _) = measure(&log, start, window);

    write_msg(&mut w, &ClientMsg::SetQuality { max_fps: 5, jpeg_quality: 20 }).await.unwrap();
    tokio::time::sleep(settle).await;
    let start = Instant::now();
    tokio::time::sleep(window).await;
    let (slow, low_quality) = measure(&log, start, window);

    write_msg(&mut w, &ClientMsg::SetQuality { max_fps: 30, jpeg_quality: 95 }).await.unwrap();
    tokio::time::sleep(settle).await;
    let start = Instant::now();
    tokio::time::sleep(window).await;
    let (fast, high_quality) = measure(&log, start, window);

    eprintln!(
        "frames in {window:?}: {normal} (host settings), {slow} (5 fps, quality 20: {low_quality} B/JPEG tile), \
         {fast} (30 fps, quality 95: {high_quality} B/JPEG tile)"
    );
    assert!(slow <= 13, "asked for 5 fps, got {slow} frames in {window:?} (host settings: {normal})");
    assert!(fast >= 2 * slow.max(1), "back to 30 fps, got {fast} frames in {window:?} (at 5 fps: {slow})");
    // Each 64×64 JPEG carries its own tables (about 600 B), so sizes differ less than the
    // pictures do; unchanged, both measured within 2% of each other.
    assert!(
        low_quality > 0 && high_quality * 10 > low_quality * 13,
        "JPEG tiles at quality 95 average {high_quality} B, at quality 20 {low_quality} B"
    );

    frames.abort();
    c.conn.close(transport::close::NORMAL, b"");
    h.shutdown().await;
}
