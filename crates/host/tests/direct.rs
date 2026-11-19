//! Direct access (no server): an unregistered host turns away connections from addresses
//! it does not allow before any login. Here nothing is allowed, so even loopback is refused.

use std::time::Duration;

use rand::rngs::OsRng;
use rdesk_common::config::{self, NetworkConfig, TransportTuning};
use rdesk_common::transport::{self, HostIdentity, Role};
use ssh_key::{Algorithm, PrivateKey};

#[tokio::test(flavor = "multi_thread")]
async fn refused_from_addresses_not_allowed() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before any thread reads the environment.
    unsafe { std::env::set_var("RDESK_CONFIG_DIR", dir.path()) };
    let id = HostIdentity::generate();
    id.save().unwrap();
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    config::write_private(
        &config::config_path(config::AUTHORIZED_KEYS).unwrap(),
        key.public_key().to_openssh().unwrap().as_bytes(),
    )
    .unwrap();
    std::fs::write(config::config_path(config::HOST_CONFIG).unwrap(), "[direct]\nfrom = []\n").unwrap();

    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let host = tokio::spawn(rdesk_host::run(rdesk_host::RunOpts {
        port: Some(port),
        fake_capture: true,
        fake_input: true,
        ..Default::default()
    }));
    tokio::time::sleep(Duration::from_millis(800)).await;

    let net = NetworkConfig::default();
    let ep = transport::bind(None, &net, &TransportTuning::default(), Role::Client).await.unwrap();
    let addr = format!("127.0.0.1:{port}");
    let r = transport::connect(&ep, id.fingerprint(), Some(&addr), &net, Duration::from_secs(3)).await;
    assert!(r.is_err(), "a connection from an address direct access does not allow must be refused");
    assert!(!host.is_finished(), "the host keeps running");
    host.abort();
}
