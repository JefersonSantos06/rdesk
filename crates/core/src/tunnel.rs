//! Port tunnels ([`StreamOpen::Tunnel`]): a TCP port on this computer that leads to a port
//! reachable from the host (its own RDP or SSH, a printer's or router's page on its network),
//! through the session's encrypted connection. Like SSH's `-L` and the TCP tunnels of other
//! remote desktops.

use anyhow::{Result, anyhow, bail};
use rdesk_common::transport::{Connection, RecvStream, SendStream};
use rdesk_proto::{MAX_SMALL_MSG, StreamOpen, TunnelReq, TunnelResp, read_msg, write_msg};
use tokio::io::AsyncWriteExt as _;
use tokio::net::{TcpListener, TcpStream};

/// What a tunnel connects: `local` port here to `host`:`port` from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelSpec {
    pub local: u16,
    pub host: String,
    pub port: u16,
}

impl TunnelSpec {
    /// `8080:localhost:80` or `8080:192.168.0.1:80` (or `[::1]` style hosts).
    pub fn parse(s: &str) -> Result<Self> {
        let (local, rest) = s.split_once(':').ok_or_else(|| anyhow!("expected LOCAL:HOST:PORT"))?;
        let (host, port) = rest.rsplit_once(':').ok_or_else(|| anyhow!("expected LOCAL:HOST:PORT"))?;
        let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
        if host.is_empty() {
            bail!("expected LOCAL:HOST:PORT");
        }
        Ok(Self { local: local.parse()?, host, port: port.parse()? })
    }
}

/// Open one tunnel stream to `host`:`port` from the host.
pub async fn open(conn: &Connection, host: &str, port: u16) -> Result<(SendStream, RecvStream)> {
    let (mut w, mut r) = conn.open_bi().await?;
    write_msg(&mut w, &StreamOpen::Tunnel).await?;
    write_msg(&mut w, &TunnelReq { host: host.to_string(), port }).await?;
    match read_msg::<_, TunnelResp>(&mut r, MAX_SMALL_MSG).await {
        Ok(Some(TunnelResp::Ok)) => Ok((w, r)),
        Ok(Some(TunnelResp::Err(e))) => bail!("{e}"),
        _ => bail!("this computer has no tunnels (an older rdesk)"),
    }
}

/// Copy between a TCP connection and a tunnel stream until either side ends.
pub async fn splice(tcp: TcpStream, mut w: SendStream, mut r: RecvStream) -> Result<()> {
    let (mut tr, mut tw) = tcp.into_split();
    let up = async {
        tokio::io::copy(&mut tr, &mut w).await?;
        let _ = w.finish();
        Ok::<_, anyhow::Error>(())
    };
    let down = async {
        tokio::io::copy(&mut r, &mut tw).await?;
        let _ = tw.shutdown().await;
        Ok::<_, anyhow::Error>(())
    };
    let (a, b) = tokio::join!(up, down);
    a.and(b)
}

/// Listen on `spec.local` (this computer only) and carry each connection through `conn`.
/// Runs until `conn` closes; `report` hears about failed connections.
pub async fn forward(
    conn: Connection,
    spec: TunnelSpec,
    report: impl Fn(String) + Send + Sync + 'static,
) -> Result<()> {
    let listener =
        TcpListener::bind(("127.0.0.1", spec.local)).await.map_err(|e| anyhow!("port {}: {e}", spec.local))?;
    let report = std::sync::Arc::new(report);
    loop {
        let (tcp, _) = tokio::select! {
            a = listener.accept() => a?,
            _ = conn.closed() => return Ok(()),
        };
        let _ = tcp.set_nodelay(true);
        let (conn, spec, report) = (conn.clone(), spec.clone(), report.clone());
        tokio::spawn(async move {
            let r = async {
                let (w, r) = open(&conn, &spec.host, spec.port).await?;
                splice(tcp, w, r).await
            }
            .await;
            if let Err(e) = r {
                report(format!("{}:{} — {e:#}", spec.host, spec.port));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs() {
        assert_eq!(
            TunnelSpec::parse("13389:localhost:3389").unwrap(),
            TunnelSpec { local: 13389, host: "localhost".into(), port: 3389 }
        );
        assert_eq!(TunnelSpec::parse("8080:[::1]:80").unwrap().host, "::1");
        assert!(TunnelSpec::parse("8080:80").is_err());
        assert!(TunnelSpec::parse("x:h:1").is_err());
    }
}
