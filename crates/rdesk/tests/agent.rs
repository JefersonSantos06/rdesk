//! The host as the Windows service starts it (`rdesk host run --agent`): it answers the app
//! on the control pipe, only lets administrators restart it, and stops when the service
//! closes its standard input.
#![cfg(windows)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rand::rngs::OsRng;
use rdesk_host::HostState;
use rdesk_host::ipc::{Request, Response, request};
use ssh_key::{Algorithm, PrivateKey};

#[tokio::test(flavor = "multi_thread")]
async fn agent_pipe_and_stop() {
    let dir = tempfile::tempdir().unwrap();
    // Some login method, or the host refuses to start.
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    std::fs::write(dir.path().join("authorized_keys"), key.public_key().to_openssh().unwrap()).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_rdesk"))
        .args(["host", "run", "--agent", "--fake-capture", "--fake-input", "--port", "0"])
        .env("RDESK_CONFIG_DIR", dir.path())
        .env_remove("RDESK_HOST_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut ready = None;
    for _ in 0..150 {
        if let Ok(Response::Status(s)) = request(&Request::Status).await
            && matches!(s.state, HostState::Listening(_))
        {
            ready = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let status = ready.expect("the agent never reported Listening on the pipe");
    assert!(status.fingerprint.is_some(), "the host ID is in the status");
    assert!(status.session.is_none());

    assert!(matches!(request(&Request::Disconnect).await.unwrap(), Response::Ok), "anyone may end the session");
    if !rdesk_host::elevation::is_elevated() {
        assert!(
            matches!(request(&Request::Restart).await.unwrap(), Response::Error(_)),
            "only administrators restart the host"
        );
    }

    // The service stops it by closing standard input.
    drop(child.stdin.take());
    let start = Instant::now();
    let code = loop {
        if let Some(code) = child.try_wait().unwrap() {
            break code;
        }
        assert!(start.elapsed() < Duration::from_secs(15), "the agent did not stop when stdin closed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(code.success(), "a requested stop exits with 0 (the service restarts it without backoff): {code:?}");
    assert!(request(&Request::Status).await.is_err(), "the pipe is gone with the host");
    assert!(dir.path().join("logs").join("host.log").exists(), "the agent logs to a file");
}

/// The helper the service's host starts as the signed-in user lists a folder as JSON.
#[test]
fn file_helper_lists_a_folder() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rdesk"))
        .args(["host", "file-helper", "list"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let listed: Result<(String, Vec<rdesk_proto::DirEntry>), String> = serde_json::from_slice(&out.stdout).unwrap();
    let (_, entries) = listed.unwrap();
    let mut names: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.is_dir)).collect();
    names.sort();
    assert_eq!(names, [("hello.txt", false), ("sub", true)]);
}
