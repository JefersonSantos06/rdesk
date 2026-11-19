//! QUIC endpoints: host identity, certificate pinning and link tuning.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{IdleTimeout, MtuDiscoveryConfig, TransportConfig, VarInt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::config::{self, Congestion, TransportTuning};

/// Label for the TLS exporter that binds authentication to this QUIC connection.
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-rdesk-auth-v1";

/// Application close codes.
pub mod close {
    use quinn::VarInt;
    pub const NORMAL: VarInt = VarInt::from_u32(0);
    pub const AUTH_FAILED: VarInt = VarInt::from_u32(1);
    pub const REPLACED: VarInt = VarInt::from_u32(2);
    pub const PROTOCOL: VarInt = VarInt::from_u32(3);
    pub const UNTRUSTED_HOST: VarInt = VarInt::from_u32(4);
    pub const SHUTDOWN: VarInt = VarInt::from_u32(5);
    /// Someone at the host ended the session; don't reconnect.
    pub const KICKED: VarInt = VarInt::from_u32(6);
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// The host's long-lived TLS identity (self-signed Ed25519 certificate).
pub struct HostIdentity {
    pub cert: CertificateDer<'static>,
    pub key: PrivatePkcs8KeyDer<'static>,
}

impl HostIdentity {
    pub fn generate() -> Result<Self> {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
        let mut params = rcgen::CertificateParams::new(vec!["rdesk".to_string()])?;
        params.distinguished_name.push(rcgen::DnType::CommonName, "rdesk host");
        // Pinned by fingerprint, so a long validity is fine and avoids surprise expiry.
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2124, 1, 1);
        let cert = params.self_signed(&key)?;
        Ok(Self { cert: cert.der().clone(), key: PrivatePkcs8KeyDer::from(key.serialize_der()) })
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        sha256(&self.cert)
    }

    pub fn load() -> Result<Self> {
        let cert = std::fs::read(config::config_path(config::HOST_CERT)?)
            .context("host certificate missing; run `rdesk host init` first")?;
        let key = std::fs::read(config::config_path(config::HOST_KEY)?)
            .context("host key missing; run `rdesk host init` first")?;
        Ok(Self { cert: CertificateDer::from(cert), key: PrivatePkcs8KeyDer::from(key) })
    }

    pub fn save(&self) -> Result<()> {
        config::write_private(&config::config_path(config::HOST_KEY)?, self.key.secret_pkcs8_der())?;
        config::write_private(&config::config_path(config::HOST_CERT)?, &self.cert)?;
        Ok(())
    }
}

pub fn transport_config(t: &TransportTuning, is_host: bool) -> Result<TransportConfig> {
    let mut tc = TransportConfig::default();
    tc.keep_alive_interval(Some(Duration::from_millis(t.keep_alive_ms.max(100))));
    tc.max_idle_timeout(Some(IdleTimeout::try_from(Duration::from_millis(t.idle_timeout_ms.max(1000)))?));
    tc.initial_mtu(1200);
    let mut mtu = MtuDiscoveryConfig::default();
    mtu.upper_bound(t.max_udp_payload.max(1200));
    tc.mtu_discovery_config(Some(mtu));
    // Video frames are bursts of several hundred KB; give streams room so a
    // frame never stalls on flow control.
    tc.stream_receive_window(VarInt::from_u32(8 * 1024 * 1024));
    tc.receive_window(VarInt::from_u32(64 * 1024 * 1024));
    tc.send_window(64 * 1024 * 1024);
    if is_host {
        // Before auth the client may only open the control stream; limits are
        // raised once it authenticates (see `raise_stream_limits`).
        tc.max_concurrent_bidi_streams(VarInt::from_u32(1));
        tc.max_concurrent_uni_streams(VarInt::from_u32(0));
    } else {
        // Host opens one uni stream per video frame.
        tc.max_concurrent_uni_streams(VarInt::from_u32(64));
        tc.max_concurrent_bidi_streams(VarInt::from_u32(0));
    }
    match t.congestion {
        Congestion::Cubic => {
            tc.congestion_controller_factory(Arc::new(quinn::congestion::CubicConfig::default()));
        }
        Congestion::Bbr => {
            tc.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
        }
        Congestion::NewReno => {
            tc.congestion_controller_factory(Arc::new(quinn::congestion::NewRenoConfig::default()));
        }
    }
    Ok(tc)
}

/// Called by the host after successful authentication.
pub fn raise_stream_limits(conn: &quinn::Connection) {
    conn.set_max_concurrent_bi_streams(VarInt::from_u32(32));
    conn.set_max_concurrent_uni_streams(VarInt::from_u32(8));
}

pub fn server_endpoint(addr: SocketAddr, id: &HostIdentity, t: &TransportTuning) -> Result<quinn::Endpoint> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![id.cert.clone()], PrivateKeyDer::Pkcs8(id.key.clone_key()))?;
    tls.alpn_protocols = vec![rdesk_proto::ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let mut sc = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    sc.transport_config(Arc::new(transport_config(t, true)?));
    // Address validation costs a round-trip and buys nothing on a tailnet.
    sc.retry_token_lifetime(Duration::from_secs(15));
    let ep = quinn::Endpoint::server(sc, addr).with_context(|| format!("binding UDP {addr}"))?;
    Ok(ep)
}

/// Accepts any certificate whose SHA-256 matches `expected` (or any at all when
/// `expected` is `None`, for trust-on-first-use), and records what it saw.
/// The TLS 1.3 handshake signature is still verified, so the peer must hold
/// the private key of the certificate it presented.
#[derive(Debug)]
struct PinnedVerifier {
    expected: Option<[u8; 32]>,
    seen: Arc<Mutex<Option<[u8; 32]>>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = sha256(end_entity);
        *self.seen.lock().unwrap() = Some(fp);
        match self.expected {
            Some(exp) if exp != fp => Err(rustls::Error::General(format!(
                "host certificate fingerprint mismatch: expected {}, got {}",
                config::format_fingerprint(&exp),
                config::format_fingerprint(&fp)
            ))),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not allowed".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

pub struct Connected {
    pub conn: quinn::Connection,
    /// SHA-256 of the host's certificate as presented in this handshake.
    pub host_fingerprint: [u8; 32],
    /// TLS exporter value for this connection (auth channel binding).
    pub ek: [u8; 32],
}

pub fn client_endpoint(remote: &SocketAddr) -> Result<quinn::Endpoint> {
    let bind: SocketAddr = if remote.is_ipv6() { "[::]:0".parse()? } else { "0.0.0.0:0".parse()? };
    Ok(quinn::Endpoint::client(bind)?)
}

/// Connect and complete the QUIC/TLS handshake. With `pin = None` any host
/// certificate is accepted; the caller must then check `host_fingerprint`
/// (trust-on-first-use) before sending anything secret.
pub async fn connect(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    pin: Option<[u8; 32]>,
    t: &TransportTuning,
    timeout: Duration,
) -> Result<Connected> {
    let seen = Arc::new(Mutex::new(None));
    let provider = provider();
    let verifier = PinnedVerifier { expected: pin, seen: seen.clone(), provider: provider.clone() };
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    tls.alpn_protocols = vec![rdesk_proto::ALPN.to_vec()];
    tls.enable_early_data = false;
    let mut cc = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    cc.transport_config(Arc::new(transport_config(t, false)?));

    let connecting = endpoint.connect_with(cc, addr, "rdesk")?;
    let conn = match tokio::time::timeout(timeout, connecting).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            let fp = *seen.lock().unwrap();
            if let (Some(exp), Some(got)) = (pin, fp)
                && exp != got
            {
                bail!(
                    "HOST IDENTITY CHANGED for {addr}!\n  expected {}\n  got      {}\n\
                     If you reinstalled the host, remove its line from known_hosts and reconnect.",
                    config::format_fingerprint(&exp),
                    config::format_fingerprint(&got)
                );
            }
            return Err(anyhow!(e).context(format!("connecting to {addr}")));
        }
        Err(_) => bail!("timed out connecting to {addr} after {timeout:?}"),
    };
    let host_fingerprint = seen.lock().unwrap().ok_or_else(|| anyhow!("no host certificate seen"))?;
    let ek = exporter(&conn)?;
    Ok(Connected { conn, host_fingerprint, ek })
}

pub fn exporter(conn: &quinn::Connection) -> Result<[u8; 32]> {
    let mut ek = [0u8; 32];
    conn.export_keying_material(&mut ek, EXPORTER_LABEL, b"").map_err(|_| anyhow!("TLS exporter unavailable"))?;
    Ok(ek)
}

/// Is `ip` inside Tailscale's address ranges (100.64.0.0/10 or fd7a:115c:a1e0::/48)?
pub fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xc0) == 64
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_tailnet(IpAddr::V4(v4));
            }
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

pub(crate) fn tailscale_cli_candidates() -> Vec<String> {
    let mut v = vec!["tailscale".to_string()];
    if cfg!(target_os = "macos") {
        v.push("/Applications/Tailscale.app/Contents/MacOS/Tailscale".into());
    }
    if cfg!(windows) {
        v.push(r"C:\Program Files\Tailscale\tailscale.exe".into());
    }
    v
}

/// Ask the local Tailscale daemon for this machine's tailnet addresses.
pub async fn tailscale_ips() -> Result<Vec<IpAddr>> {
    let mut last_err = None;
    for cli in tailscale_cli_candidates() {
        match crate::command(&cli).arg("ip").output().await {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                let ips: Vec<IpAddr> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
                if ips.is_empty() {
                    bail!("`tailscale ip` returned no addresses (is Tailscale logged in?)");
                }
                return Ok(ips);
            }
            Ok(out) => last_err = Some(anyhow!("`{cli} ip` failed: {}", String::from_utf8_lossy(&out.stderr).trim())),
            Err(e) => last_err = Some(anyhow!("running `{cli}`: {e}")),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("tailscale CLI not found")))
}

/// Resolve `host[:port]` (IPs, hostnames and MagicDNS names).
pub async fn resolve(addr: &str, default_port: u16) -> Result<SocketAddr> {
    if let Ok(sa) = addr.parse::<SocketAddr>() {
        return Ok(sa);
    }
    if let Ok(ip) = addr.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, default_port));
    }
    let with_port = if addr.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) && !addr.contains("::") {
        addr.to_string()
    } else {
        format!("{addr}:{default_port}")
    };
    let mut addrs = tokio::net::lookup_host(&with_port).await.with_context(|| format!("resolving {addr}"))?;
    addrs.next().ok_or_else(|| anyhow!("{addr} did not resolve to any address"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailnet_ranges() {
        assert!(is_tailnet("100.64.0.1".parse().unwrap()));
        assert!(is_tailnet("100.127.255.254".parse().unwrap()));
        assert!(!is_tailnet("100.128.0.1".parse().unwrap()));
        assert!(!is_tailnet("100.63.255.255".parse().unwrap()));
        assert!(!is_tailnet("192.168.1.10".parse().unwrap()));
        assert!(is_tailnet("fd7a:115c:a1e0::1234".parse().unwrap()));
        assert!(!is_tailnet("fd7a:115c:a1e1::1".parse().unwrap()));
        assert!(is_tailnet("::ffff:100.100.1.1".parse().unwrap()));
    }

    #[tokio::test]
    async fn resolve_forms() {
        assert_eq!(resolve("127.0.0.1", 7420).await.unwrap(), "127.0.0.1:7420".parse().unwrap());
        assert_eq!(resolve("127.0.0.1:9", 7420).await.unwrap(), "127.0.0.1:9".parse().unwrap());
        assert_eq!(resolve("::1", 7420).await.unwrap(), "[::1]:7420".parse().unwrap());
        assert_eq!(resolve("localhost:9", 7420).await.unwrap().port(), 9);
    }
}
