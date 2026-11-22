//! The host's end of a port tunnel ([`rdesk_proto::StreamOpen::Tunnel`]): connect to the
//! address asked for (this computer or its network) and pass the bytes along. Full control
//! only: it reaches no further than a terminal on this computer already does.

use std::time::Duration;

use anyhow::Result;
use rdesk_common::transport::{RecvStream, SendStream};
use rdesk_proto::{MAX_SMALL_MSG, TunnelReq, TunnelResp, expect_msg, write_msg};
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpStream;

pub async fn serve(r: &mut RecvStream, w: &mut SendStream, allowed: bool, report: impl FnOnce(&str)) -> Result<()> {
    let req: TunnelReq = expect_msg(r, MAX_SMALL_MSG).await?;
    if !allowed {
        let why = rdesk_common::t!("a tunnel needs full control", "um túnel precisa de controle total");
        write_msg(w, &TunnelResp::Err(why.into())).await?;
        return Ok(());
    }
    let target =
        format!("{}:{}", if req.host.contains(':') { format!("[{}]", req.host) } else { req.host.clone() }, req.port);
    let tcp = match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&target)).await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            write_msg(w, &TunnelResp::Err(format!("{target}: {e}"))).await?;
            return Ok(());
        }
        Err(_) => {
            write_msg(w, &TunnelResp::Err(format!("{target}: no answer"))).await?;
            return Ok(());
        }
    };
    let _ = tcp.set_nodelay(true);
    write_msg(w, &TunnelResp::Ok).await?;
    report(&target);
    let (mut tr, mut tw) = tcp.into_split();
    let up = async {
        tokio::io::copy(r, &mut tw).await?;
        let _ = tw.shutdown().await;
        Ok::<_, anyhow::Error>(())
    };
    let down = async {
        tokio::io::copy(&mut tr, w).await?;
        Ok::<_, anyhow::Error>(())
    };
    let (a, b) = tokio::join!(up, down);
    a.and(b)
}
