//! Networking on iroh: QUIC with hole punching and an optional private relay, the host
//! identity, and link tuning.
//!
//! A host is addressed by its **ID**, the Ed25519 public key of its endpoint. iroh's TLS
//! authenticates every connection against that key, so there is nothing to pin or to
//! trust on first use: dialing an ID only ever reaches the machine holding its secret key.
//!
//! How a client finds the host:
//! * **Relay** (the rdesk server's, when signed in): both sides keep a connection to it; the first
//!   packets go through it and iroh switches to a direct path once hole punching succeeds.
//! * **Address** (the host entry's `addr`): tried directly — LAN, port forwarding, IPv6.
//!
//! Nothing is published anywhere else: no public DNS, no third-party relays.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
pub use iroh::Endpoint;
use iroh::endpoint::{BindOpts, IdleTimeout, MtuDiscoveryConfig, QuicTransportConfig, presets};
pub use iroh::endpoint::{Connection, ConnectionError, Incoming, IncomingAddr, RecvStream, SendStream, VarInt};
use iroh::{EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey};

use crate::config::{self, Congestion, NetworkConfig, TransportTuning};

/// Label for the TLS exporter that binds authentication to this QUIC connection.
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-rdesk-auth-v1";

/// Start of the error when the computer that answered holds another ID (callers treat it
/// as final: retrying cannot help).
pub const WRONG_HOST_ID: &str = "wrong host ID";

/// Application close codes.
pub mod close {
    use iroh::endpoint::VarInt;
    pub const NORMAL: VarInt = VarInt::from_u32(0);
    pub const AUTH_FAILED: VarInt = VarInt::from_u32(1);
    pub const REPLACED: VarInt = VarInt::from_u32(2);
    pub const PROTOCOL: VarInt = VarInt::from_u32(3);
    pub const SHUTDOWN: VarInt = VarInt::from_u32(5);
    /// Someone at the host ended the session; don't reconnect.
    pub const KICKED: VarInt = VarInt::from_u32(6);
}

/// The host's long-lived identity: the secret key of its endpoint. Its public half is the
/// host ID that clients dial.
pub struct HostIdentity {
    key: SecretKey,
}

impl HostIdentity {
    pub fn generate() -> Self {
        Self { key: SecretKey::generate() }
    }

    /// The host ID (public key) as raw bytes.
    pub fn fingerprint(&self) -> [u8; 32] {
        *self.key.public().as_bytes()
    }

    pub fn secret_key(&self) -> SecretKey {
        self.key.clone()
    }

    pub fn load() -> Result<Self> {
        let bytes = std::fs::read(config::config_path(config::HOST_KEY)?)
            .context("host identity missing; run `rdesk host init` first")?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| anyhow!("{} is not a 32-byte key", config::HOST_KEY))?;
        Ok(Self { key: SecretKey::from_bytes(&bytes) })
    }

    pub fn save(&self) -> Result<()> {
        config::write_private(&config::config_path(config::HOST_KEY)?, &self.key.to_bytes())
    }
}

/// QUIC settings shared by both sides (iroh's defaults otherwise).
pub fn transport_config(t: &TransportTuning, is_host: bool) -> Result<QuicTransportConfig> {
    let mut mtu = MtuDiscoveryConfig::default();
    mtu.upper_bound(t.max_udp_payload.max(1200));
    let mut b = QuicTransportConfig::builder()
        .keep_alive_interval(Duration::from_millis(t.keep_alive_ms.max(100)))
        .max_idle_timeout(Some(IdleTimeout::try_from(Duration::from_millis(t.idle_timeout_ms.max(1000)))?))
        .mtu_discovery_config(Some(mtu))
        // Video frames are bursts of several hundred KB; give streams room so a
        // frame never stalls on flow control.
        .stream_receive_window(VarInt::from_u32(8 * 1024 * 1024))
        .receive_window(VarInt::from_u32(64 * 1024 * 1024))
        .send_window(64 * 1024 * 1024);
    b = if is_host {
        // Before auth the client may only open the control stream; limits are
        // raised once it authenticates (see `raise_stream_limits`).
        b.max_concurrent_bidi_streams(VarInt::from_u32(1)).max_concurrent_uni_streams(VarInt::from_u32(0))
    } else {
        // Host opens one uni stream per video frame.
        b.max_concurrent_uni_streams(VarInt::from_u32(64)).max_concurrent_bidi_streams(VarInt::from_u32(0))
    };
    use noq_proto::congestion;
    b = match t.congestion {
        Congestion::Cubic => b.congestion_controller_factory(Arc::new(congestion::CubicConfig::default())),
        Congestion::Bbr => b.congestion_controller_factory(Arc::new(congestion::Bbr3Config::default())),
        Congestion::NewReno => b.congestion_controller_factory(Arc::new(congestion::NewRenoConfig::default())),
    };
    Ok(b.build())
}

/// Called by the host after successful authentication.
pub fn raise_stream_limits(conn: &Connection) {
    conn.set_max_concurrent_bi_streams(VarInt::from_u32(32));
    conn.set_max_concurrent_uni_streams(VarInt::from_u32(8));
}

/// The relay of the server this computer is signed in to, if any.
pub fn relay_url(net: &NetworkConfig) -> Result<Option<RelayUrl>> {
    match net.relay.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(u) => {
            let with_scheme = if u.contains("://") { u.to_string() } else { format!("https://{u}") };
            Ok(Some(with_scheme.parse().with_context(|| format!("invalid relay URL {u:?}"))?))
        }
    }
}

fn relay_mode(net: &NetworkConfig) -> Result<RelayMode> {
    Ok(match relay_url(net)? {
        Some(url) => {
            let map = RelayMap::from_iter([url]);
            // The relay only lets in signed-in endpoints.
            RelayMode::Custom(match &net.token {
                Some(t) => map.with_auth_token(t.clone()),
                None => map,
            })
        }
        None => RelayMode::Disabled,
    })
}

/// Which side of a connection an endpoint is for.
#[derive(Debug, Clone, Copy)]
pub enum Role {
    /// Accepts sessions; also listens for direct connections on `port` (0 = any).
    Host { port: u16 },
    /// Dials hosts from a random port.
    Client,
}

/// Create an endpoint. `key` is the host identity (clients pass `None`: a fresh key
/// per process, since only the host's ID matters to rdesk).
pub async fn bind(key: Option<SecretKey>, net: &NetworkConfig, t: &TransportTuning, role: Role) -> Result<Endpoint> {
    let is_host = matches!(role, Role::Host { .. });
    let mut b = Endpoint::builder(presets::Minimal)
        .relay_mode(relay_mode(net)?)
        .transport_config(transport_config(t, is_host)?);
    if let Some(k) = key {
        b = b.secret_key(k);
    }
    if let Role::Host { port } = role {
        b = b
            .alpns(vec![rdesk_proto::ALPN.to_vec()])
            .clear_ip_transports()
            .bind_addr(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))?
            .bind_addr_with_opts(
                SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0),
                BindOpts::default().set_is_required(false),
            )?;
    }
    b.bind().await.map_err(|e| {
        let text = format!("{e:#} {:?}", std::error::Error::source(&e));
        if text.contains("10048") || text.to_lowercase().contains("in use") {
            anyhow!("UDP port {} is already in use (is another rdesk host running?)", host_port(role))
        } else {
            anyhow!(e).context("starting the network endpoint")
        }
    })
}

fn host_port(role: Role) -> u16 {
    match role {
        Role::Host { port } => port,
        Role::Client => 0,
    }
}

/// Wait (bounded) until the endpoint is registered with its relay. `Ok(false)` without one.
pub async fn wait_relay(ep: &Endpoint, net: &NetworkConfig, timeout: Duration) -> Result<bool> {
    if relay_url(net)?.is_none() {
        return Ok(false);
    }
    tokio::time::timeout(timeout, ep.online()).await.map_err(|_| anyhow!("relay did not answer within {timeout:?}"))?;
    Ok(true)
}

pub struct Connected {
    pub conn: Connection,
    /// The host's ID, as authenticated by this handshake.
    pub host_fingerprint: [u8; 32],
    /// TLS exporter value for this connection (auth channel binding).
    pub ek: [u8; 32],
}

/// Connect to the host `id`, through the relay and/or `direct` (`host[:port]`).
pub async fn connect(
    endpoint: &Endpoint,
    id: [u8; 32],
    direct: Option<&str>,
    net: &NetworkConfig,
    timeout: Duration,
) -> Result<Connected> {
    let host =
        EndpointId::from_bytes(&id).map_err(|_| anyhow!("{} is not a valid host ID", config::format_host_id(&id)))?;
    let mut addr = EndpointAddr::new(host);
    let relay = relay_url(net)?;
    if let Some(url) = relay.clone() {
        addr = addr.with_relay_url(url);
    }
    if let Some(d) = direct.map(str::trim).filter(|d| !d.is_empty()) {
        match resolve_all(d, rdesk_proto::DEFAULT_PORT).await {
            Ok(list) => {
                for sa in list {
                    addr = addr.with_ip_addr(sa);
                }
            }
            // With a relay the address is only a shortcut.
            Err(e) if relay.is_some() => tracing::debug!("{e:#}"),
            Err(e) => return Err(e),
        }
    }
    if addr.is_empty() {
        bail!("no way to reach this computer: sign in to your rdesk server (rdesk login), or give its address");
    }
    let conn = match tokio::time::timeout(timeout, endpoint.connect(addr, rdesk_proto::ALPN)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            let e = anyhow!(e);
            if format!("{e:#}").contains("invalid peer certificate") {
                bail!(
                    "{WRONG_HOST_ID}: the computer that answered is not {} (check the ID and the address)",
                    host.fmt_short()
                );
            }
            return Err(e.context(format!("connecting to {}", host.fmt_short())));
        }
        Err(_) => bail!(
            "no answer from {} within {timeout:?} (is its host on? check the ID{})",
            host.fmt_short(),
            if relay.is_some() { " and the relay" } else { " and the address" }
        ),
    };
    let ek = exporter(&conn)?;
    Ok(Connected { host_fingerprint: *conn.remote_id().as_bytes(), conn, ek })
}

pub fn exporter(conn: &Connection) -> Result<[u8; 32]> {
    let mut ek = [0u8; 32];
    conn.export_keying_material(&mut ek, EXPORTER_LABEL, b"").map_err(|_| anyhow!("TLS exporter unavailable"))?;
    Ok(ek)
}

/// How the traffic currently flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Direct,
    Relay,
}

impl Route {
    pub fn label(self) -> &'static str {
        match self {
            Route::Direct => "direct",
            Route::Relay => "relay",
        }
    }
}

/// RTT and route of the path carrying the traffic.
pub fn path_info(conn: &Connection) -> (Duration, Option<Route>) {
    let paths = conn.paths();
    let mut chosen = None;
    for p in paths.iter() {
        if p.is_selected() || chosen.is_none() {
            chosen = Some((p.rtt(), if p.is_relay() { Route::Relay } else { Route::Direct }));
        }
        if p.is_selected() {
            break;
        }
    }
    match chosen {
        Some((rtt, route)) => (rtt, Some(route)),
        None => (Duration::ZERO, None),
    }
}

pub fn rtt(conn: &Connection) -> Duration {
    path_info(conn).0
}

/// A short description of who is on the other end, for logs.
pub fn describe_peer(conn: &Connection) -> String {
    let (_, route) = path_info(conn);
    format!("{} ({})", conn.remote_id().fmt_short(), route.map_or("?", Route::label))
}

/// Resolve `host[:port]` (IPs and hostnames).
pub async fn resolve(addr: &str, default_port: u16) -> Result<SocketAddr> {
    resolve_all(addr, default_port).await?.into_iter().next().ok_or_else(|| anyhow!("{addr} did not resolve"))
}

async fn resolve_all(addr: &str, default_port: u16) -> Result<Vec<SocketAddr>> {
    if let Ok(sa) = addr.parse::<SocketAddr>() {
        return Ok(vec![sa]);
    }
    if let Ok(ip) = addr.trim_matches(['[', ']']).parse::<std::net::IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, default_port)]);
    }
    let with_port = if addr.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) && !addr.contains("::") {
        addr.to_string()
    } else {
        format!("{addr}:{default_port}")
    };
    let list: Vec<SocketAddr> =
        tokio::net::lookup_host(&with_port).await.with_context(|| format!("resolving {addr}"))?.collect();
    if list.is_empty() {
        bail!("{addr} did not resolve to any address");
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_forms() {
        assert_eq!(resolve("127.0.0.1", 7420).await.unwrap(), "127.0.0.1:7420".parse().unwrap());
        assert_eq!(resolve("127.0.0.1:9", 7420).await.unwrap(), "127.0.0.1:9".parse().unwrap());
        assert_eq!(resolve("::1", 7420).await.unwrap(), "[::1]:7420".parse().unwrap());
        assert_eq!(resolve("localhost:9", 7420).await.unwrap().port(), 9);
    }

    #[test]
    fn relay_urls() {
        let n = |r: &str| NetworkConfig { relay: Some(r.into()), token: None };
        assert!(relay_url(&NetworkConfig::default()).unwrap().is_none());
        assert!(relay_url(&n("  ")).unwrap().is_none());
        assert_eq!(relay_url(&n("relay.example.com")).unwrap().unwrap().as_str(), "https://relay.example.com/");
        assert_eq!(relay_url(&n("http://10.0.0.2:3340")).unwrap().unwrap().as_str(), "http://10.0.0.2:3340/");
    }

    #[test]
    fn identity_roundtrip() {
        let id = HostIdentity::generate();
        let again = HostIdentity { key: SecretKey::from_bytes(&id.key.to_bytes()) };
        assert_eq!(id.fingerprint(), again.fingerprint());
        assert!(config::parse_host_id(&config::format_host_id(&id.fingerprint())).is_ok());
    }
}
