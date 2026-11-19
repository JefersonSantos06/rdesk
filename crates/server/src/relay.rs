//! The embedded iroh relay. It listens on loopback over plain HTTP and the public HTTPS
//! server proxies `/relay` and `/ping` to it, so everything shares port 443 and one
//! certificate. Only endpoints presenting a valid session token get in.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use iroh_base::EndpointId;
use iroh_relay::server::{Access, AccessControl, ClientRequest, ConnectionId, QuicConfig, RelayConfig, ServerConfig};

use crate::db::Db;
use crate::state::Shared;

/// Endpoints currently connected to the relay (how many connections each).
#[derive(Debug, Default, Clone)]
pub struct Online(Arc<Mutex<HashMap<EndpointId, usize>>>);

impl Online {
    pub fn contains(&self, id: &EndpointId) -> bool {
        self.0.lock().unwrap().contains_key(id)
    }
}

#[derive(Debug)]
struct SessionAccess {
    db: Db,
    online: Online,
}

impl AccessControl for SessionAccess {
    async fn on_connect(&self, req: &ClientRequest) -> Access {
        let ok = match req.auth_token() {
            Some(t) => matches!(self.db.session(&t), Ok(Some(_))),
            None => false,
        };
        if !ok {
            tracing::debug!("relay: refused {}", req.endpoint_id().fmt_short());
            return Access::Deny { reason: Some("sign in to the rdesk server".into()) };
        }
        *self.online.0.lock().unwrap().entry(req.endpoint_id()).or_default() += 1;
        Access::Allow
    }

    fn on_disconnect(&self, id: EndpointId, _: ConnectionId) {
        let mut m = self.online.0.lock().unwrap();
        if let Some(n) = m.get_mut(&id) {
            *n -= 1;
            if *n == 0 {
                m.remove(&id);
            }
        }
    }
}

pub struct Relay {
    pub server: iroh_relay::server::Server,
    pub http: SocketAddr,
}

/// Start the relay; `quic` enables QUIC address discovery (needs a TLS config).
pub async fn spawn(db: Db, online: Online, quic: Option<(SocketAddr, rustls::ServerConfig)>) -> Result<Relay> {
    let mut relay = RelayConfig::new((Ipv4Addr::LOCALHOST, 0));
    relay.access = Arc::new(SessionAccess { db, online: online.clone() });
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(relay);
    if let Some((addr, tls)) = quic {
        let mut q = QuicConfig::new(addr);
        q.server_config = Some(tls);
        cfg.quic = Some(q);
    }
    let server = iroh_relay::server::Server::spawn(cfg).await.context("starting the relay")?;
    let http = server.http_addr().context("relay has no HTTP address")?;
    Ok(Relay { server, http })
}

pub type ProxyClient = Client<HttpConnector, Body>;

pub fn proxy_client() -> ProxyClient {
    Client::builder(TokioExecutor::new()).build_http()
}

/// Forward a request (including the WebSocket upgrade of `/relay`) to the relay.
pub async fn proxy(State(st): State<Shared>, mut req: Request) -> Response {
    let Some(target) = *st.relay_http.lock().unwrap() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "relay not running").into_response();
    };
    let path = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let wants_upgrade = req.headers().contains_key(header::UPGRADE);
    let downstream = wants_upgrade.then(|| hyper::upgrade::on(&mut req));
    match format!("http://{target}{path}").parse() {
        Ok(uri) => *req.uri_mut() = uri,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    }
    let mut resp = match st.proxy.request(req).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("relay proxy: {e}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS
        && let Some(down) = downstream
    {
        let up = hyper::upgrade::on(&mut resp);
        tokio::spawn(async move {
            match (up.await, down.await) {
                (Ok(up), Ok(down)) => {
                    let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(up), &mut TokioIo::new(down)).await;
                }
                (Err(e), _) | (_, Err(e)) => tracing::debug!("relay upgrade failed: {e}"),
            }
        });
    }
    resp.map(Body::new)
}
