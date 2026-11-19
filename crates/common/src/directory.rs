//! Finding machines: the devices Tailscale knows about, and a cheap "is rdesk
//! running there?" probe that does not log in.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use rdesk_proto::{ClientMsg, HostMsg, MAX_SMALL_MSG, expect_msg, write_msg};
use serde::Deserialize;

use crate::config::TransportTuning;
use crate::transport::{self, close};

/// One device of the tailnet, as `tailscale status --json` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct TailnetDevice {
    /// Machine name (`HostName`).
    pub name: String,
    /// MagicDNS name without the trailing dot, e.g. `casa-pc.tail1234.ts.net`.
    pub dns_name: String,
    pub os: String,
    pub ips: Vec<IpAddr>,
    pub online: bool,
    /// Traffic goes peer-to-peer (otherwise through a DERP relay).
    pub direct: bool,
    /// DERP region used as home/relay, e.g. `sao`.
    pub relay: String,
    /// Unix seconds of the last time the control server saw it; `None` when online or unknown.
    pub last_seen: Option<i64>,
    pub is_self: bool,
}

impl TailnetDevice {
    /// The address rdesk should use: the IPv4 tailnet address when there is one.
    pub fn ip(&self) -> Option<IpAddr> {
        self.ips.iter().find(|i| i.is_ipv4()).or(self.ips.first()).copied()
    }

    /// Does `addr` (as typed in a saved host) name this device?
    pub fn matches(&self, addr: &str) -> bool {
        let host = strip_port(addr).trim_end_matches('.');
        if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
            return self.ips.contains(&ip);
        }
        let host = host.to_ascii_lowercase();
        let dns = self.dns_name.to_ascii_lowercase();
        !host.is_empty()
            && (dns == host || dns.split('.').next() == Some(host.as_str()) || self.name.eq_ignore_ascii_case(&host))
    }
}

fn strip_port(addr: &str) -> &str {
    // `[v6]:port`, `v4:port`, `name:port`; a bare IPv6 has several colons and no port.
    if let Some(rest) = addr.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match addr.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && p.parse::<u16>().is_ok() => h,
        _ => addr,
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tailnet {
    /// `BackendState`, e.g. `Running`, `Stopped`, `NeedsLogin`.
    pub state: String,
    pub devices: Vec<TailnetDevice>,
}

impl Tailnet {
    pub fn running(&self) -> bool {
        self.state == "Running"
    }

    pub fn me(&self) -> Option<&TailnetDevice> {
        self.devices.iter().find(|d| d.is_self)
    }

    pub fn find(&self, addr: &str) -> Option<&TailnetDevice> {
        self.devices.iter().find(|d| d.matches(addr))
    }
}

#[derive(Deserialize)]
struct RawStatus {
    #[serde(rename = "BackendState", default)]
    backend_state: String,
    #[serde(rename = "Self")]
    me: Option<RawPeer>,
    #[serde(rename = "Peer", default)]
    peer: Option<HashMap<String, RawPeer>>,
}

#[derive(Deserialize)]
struct RawPeer {
    #[serde(rename = "HostName", default)]
    host_name: String,
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(rename = "OS", default)]
    os: String,
    #[serde(rename = "TailscaleIPs", default)]
    ips: Option<Vec<IpAddr>>,
    #[serde(rename = "Online", default)]
    online: bool,
    #[serde(rename = "CurAddr", default)]
    cur_addr: String,
    #[serde(rename = "Relay", default)]
    relay: String,
    #[serde(rename = "LastSeen", default)]
    last_seen: String,
}

impl RawPeer {
    fn into_device(self, is_self: bool) -> TailnetDevice {
        TailnetDevice {
            name: self.host_name,
            dns_name: self.dns_name.trim_end_matches('.').to_string(),
            os: self.os,
            ips: self.ips.unwrap_or_default(),
            online: self.online || is_self,
            direct: !self.cur_addr.is_empty(),
            relay: self.relay,
            last_seen: parse_rfc3339(&self.last_seen).filter(|&t| t > 0),
            is_self,
        }
    }
}

pub fn parse_status(json: &[u8]) -> Result<Tailnet> {
    let raw: RawStatus = serde_json::from_slice(json)?;
    let mut devices: Vec<TailnetDevice> = Vec::new();
    if let Some(me) = raw.me {
        devices.push(me.into_device(true));
    }
    let mut peers: Vec<TailnetDevice> =
        raw.peer.unwrap_or_default().into_values().map(|p| p.into_device(false)).collect();
    peers.sort_by_key(|d| d.name.to_lowercase());
    devices.extend(peers);
    Ok(Tailnet { state: raw.backend_state, devices })
}

/// Ask the local Tailscale daemon for the tailnet's devices.
pub async fn tailnet_status() -> Result<Tailnet> {
    let mut last_err = None;
    for cli in transport::tailscale_cli_candidates() {
        match crate::command(&cli).args(["status", "--json"]).output().await {
            Ok(out) if out.status.success() => return parse_status(&out.stdout),
            Ok(out) => {
                last_err = Some(anyhow!("`tailscale status` failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
            }
            Err(e) => last_err = Some(anyhow!("running `{cli}`: {e}")),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("tailscale CLI not found")))
}

/// What a host says about itself before anyone logs in.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeInfo {
    /// Certificate fingerprint presented in this connection (compare with known_hosts).
    pub fingerprint: [u8; 32],
    pub host_name: String,
    pub os: String,
    pub version: String,
    pub proto: u16,
    pub in_session: bool,
    pub rtt: Duration,
}

/// Is rdesk running at `addr`? Completes the QUIC handshake (any certificate is accepted:
/// nothing secret is sent) and asks the host for its status.
pub async fn probe(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    tuning: &TransportTuning,
    timeout: Duration,
) -> Result<ProbeInfo> {
    let c = transport::connect(endpoint, addr, None, tuning, timeout).await?;
    let answer = tokio::time::timeout(timeout, async {
        let (mut w, mut r) = c.conn.open_bi().await?;
        write_msg(&mut w, &ClientMsg::Probe).await?;
        let _ = w.finish();
        expect_msg::<_, HostMsg>(&mut r, MAX_SMALL_MSG).await.map_err(anyhow::Error::from)
    })
    .await;
    let rtt = c.conn.rtt();
    c.conn.close(close::NORMAL, b"probe");
    match answer {
        Ok(Ok(HostMsg::ProbeInfo { proto, host_name, os, version, in_session })) => {
            Ok(ProbeInfo { fingerprint: c.host_fingerprint, host_name, os, version, proto, in_session, rtt })
        }
        Ok(Ok(HostMsg::AuthFail { .. })) => bail!("the host runs an older rdesk that cannot report its status"),
        Ok(Ok(other)) => bail!("unexpected answer {other:?}"),
        Ok(Err(e)) => Err(e),
        Err(_) => bail!("no answer within {timeout:?}"),
    }
}

/// Seconds since the Unix epoch for `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)`.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "Z" | "z" | "" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let oh: i64 = rest.get(1..3)?.parse().ok()?;
            let om: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    // Days from civil (Howard Hinnant).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
pub fn ago(then: i64, now: i64) -> String {
    let s = (now - then).max(0);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86_400 => format!("{} h ago", s / 3600),
        _ => {
            let d = s / 86_400;
            if d == 1 { "1 day ago".into() } else { format!("{d} days ago") }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "BackendState": "Running",
        "Self": {"HostName": "DESKTOP-1", "DNSName": "desktop-1.tail.ts.net.", "OS": "windows",
                 "TailscaleIPs": ["100.97.96.81", "fd7a:115c:a1e0::1"], "Online": false,
                 "CurAddr": "", "Relay": "sao", "LastSeen": "0001-01-01T00:00:00Z"},
        "Peer": {
            "k1": {"HostName": "work-mac", "DNSName": "work-mac.tail.ts.net.", "OS": "macOS",
                   "TailscaleIPs": ["100.64.1.2"], "Online": true, "CurAddr": "1.2.3.4:41641",
                   "Relay": "sao", "LastSeen": "0001-01-01T00:00:00Z"},
            "k2": {"HostName": "Box", "DNSName": "box.tail.ts.net.", "OS": "linux",
                   "TailscaleIPs": null, "Online": false, "CurAddr": "", "Relay": "",
                   "LastSeen": "2026-11-18T20:31:05.123Z", "Unknown": 1}
        }
    }"#;

    #[test]
    fn parses_status() {
        let t = parse_status(SAMPLE.as_bytes()).unwrap();
        assert!(t.running());
        assert_eq!(t.devices.len(), 3);
        let me = t.me().unwrap();
        assert!(me.online && me.is_self);
        assert_eq!(me.ip(), Some("100.97.96.81".parse().unwrap()));
        assert_eq!(t.devices[1].name, "Box");
        assert!(t.devices[1].ips.is_empty() && !t.devices[1].online);
        assert_eq!(t.devices[1].last_seen, parse_rfc3339("2026-11-18T20:31:05Z"));
        let mac = &t.devices[2];
        assert!(mac.direct && mac.online && mac.last_seen.is_none());
        assert_eq!(mac.dns_name, "work-mac.tail.ts.net");
    }

    #[test]
    fn matches_addresses() {
        let t = parse_status(SAMPLE.as_bytes()).unwrap();
        assert_eq!(t.find("100.64.1.2").unwrap().name, "work-mac");
        assert_eq!(t.find("100.64.1.2:7420").unwrap().name, "work-mac");
        assert_eq!(t.find("work-mac.tail.ts.net").unwrap().name, "work-mac");
        assert_eq!(t.find("WORK-MAC").unwrap().name, "work-mac");
        assert_eq!(t.find("work-mac:7421").unwrap().name, "work-mac");
        assert_eq!(t.find("[fd7a:115c:a1e0::1]:7420").unwrap().name, "DESKTOP-1");
        assert_eq!(t.find("fd7a:115c:a1e0::1").unwrap().name, "DESKTOP-1");
        assert!(t.find("other").is_none());
        assert!(t.find("").is_none());
    }

    #[test]
    fn times() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_rfc3339("2026-11-18T20:31:05.5-03:00"), parse_rfc3339("2026-11-18T23:31:05Z"));
        assert!(parse_rfc3339("0001-01-01T00:00:00Z").unwrap() < 0);
        assert_eq!(parse_rfc3339("garbage"), None);
        assert_eq!(ago(100, 130), "just now");
        assert_eq!(ago(0, 7200), "2 h ago");
        assert_eq!(ago(0, 86_400 * 3), "3 days ago");
    }
}
