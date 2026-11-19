//! "Is rdesk running there?": a cheap probe that completes the handshake with a host's ID
//! and asks for its status without logging in.

use std::time::Duration;

use anyhow::{Result, bail};
use rdesk_proto::{ClientMsg, HostMsg, MAX_SMALL_MSG, expect_msg, write_msg};

use crate::config::NetworkConfig;
use crate::transport::{self, Endpoint, Route, close};

/// What a host says about itself before anyone logs in.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeInfo {
    pub host_name: String,
    pub os: String,
    pub version: String,
    pub proto: u16,
    pub in_session: bool,
    pub rtt: Duration,
    /// How the probe reached it (direct or through the relay).
    pub route: Option<Route>,
}

/// Is the host `id` up? Nothing secret is sent: iroh already proved the host holds the ID's key.
pub async fn probe(
    endpoint: &Endpoint,
    id: [u8; 32],
    direct: Option<&str>,
    net: &NetworkConfig,
    timeout: Duration,
) -> Result<ProbeInfo> {
    let c = transport::connect(endpoint, id, direct, net, timeout).await?;
    let answer = tokio::time::timeout(timeout, async {
        let (mut w, mut r) = c.conn.open_bi().await?;
        write_msg(&mut w, &ClientMsg::Probe).await?;
        let _ = w.finish();
        expect_msg::<_, HostMsg>(&mut r, MAX_SMALL_MSG).await.map_err(anyhow::Error::from)
    })
    .await;
    let (rtt, route) = transport::path_info(&c.conn);
    c.conn.close(close::NORMAL, b"probe");
    match answer {
        Ok(Ok(HostMsg::ProbeInfo { proto, host_name, os, version, in_session })) => {
            Ok(ProbeInfo { host_name, os, version, proto, in_session, rtt, route })
        }
        Ok(Ok(HostMsg::AuthFail { .. })) => bail!("the host runs an older rdesk that cannot report its status"),
        Ok(Ok(other)) => bail!("unexpected answer {other:?}"),
        Ok(Err(e)) => Err(e),
        Err(_) => bail!("no answer within {timeout:?}"),
    }
}

/// "just now", "5 min ago", "3 h ago", "2 days ago" (Unix seconds).
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

    #[test]
    fn ago_texts() {
        assert_eq!(ago(100, 130), "just now");
        assert_eq!(ago(0, 7200), "2 h ago");
        assert_eq!(ago(0, 86_400), "1 day ago");
        assert_eq!(ago(0, 86_400 * 3), "3 days ago");
    }
}
