//! Keeping a registered computer in step with its server: report addresses and whether
//! it is in use, and receive revocations (who lost access) as soon as they happen, through
//! a long poll authenticated with the machine's own token.

use std::sync::Arc;
use std::time::Duration;

use rdesk_common::account::{NotSignedIn, Registration};
use rdesk_common::transport::Endpoint;
use rdesk_proto::api::AgentSync;

use crate::control::Reporter;
use crate::server::Server;

pub(crate) async fn run(reg: Registration, ep: Endpoint, server: Arc<Server>, reporter: Reporter) {
    let api = match reg.api() {
        Ok(a) => a,
        Err(e) => {
            reporter.registration(Some(Err(format!("{e:#}"))));
            return;
        }
    };
    let Some(policy) = server.ticket.clone() else { return };
    let label = format!("{} on {}", reg.name, reg.server_name);
    let mut cursor = 0;
    let mut backoff = Duration::from_secs(2);
    let mut reported_error = false;
    loop {
        let addrs: Vec<String> = ep.addr().ip_addrs().map(|a| a.to_string()).collect();
        let sessions: Vec<_> = std::mem::take(&mut *server.records.lock().unwrap());
        let req = AgentSync {
            cursor,
            addrs,
            os: std::env::consts::OS.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            in_session: server.in_session().await,
            sessions,
            server_key: policy.server_keys.read().unwrap().first().map(hex::encode).unwrap_or_default(),
            direct: server.direct_on(),
        };
        // Sent again next time unless the server confirms them.
        let requeue = |server: &Server, req: AgentSync| {
            let mut q = server.records.lock().unwrap();
            let mut back = req.sessions;
            back.append(&mut q);
            *q = back;
        };
        // A session starting or ending is reported right away (the poll may be waiting).
        let reply = tokio::select! {
            r = api.agent_sync(&req) => r,
            _ = server.sync_now.notified() => {
                requeue(&server, req);
                continue;
            }
        };
        match reply {
            Ok(reply) => {
                {
                    let mut map = policy.revoked.write().unwrap();
                    map.clear();
                    map.extend(reply.revocations.iter().map(|r| (r.user_id, r.before)));
                }
                // Key rotation: trust what the server lists (current first) and remember it.
                let keys: Vec<[u8; 32]> = reply
                    .server_keys
                    .iter()
                    .filter_map(|k| hex::decode(k).ok().and_then(|b| b.try_into().ok()))
                    .collect();
                if !keys.is_empty() && *policy.server_keys.read().unwrap() != keys {
                    *policy.server_keys.write().unwrap() = keys.clone();
                    if let Some(mut r) = Registration::load()
                        && r.server_key != hex::encode(keys[0])
                    {
                        r.server_key = hex::encode(keys[0]);
                        let _ = r.save();
                        reporter.event("The server's ticket key changed (rotation)", false);
                    }
                }
                server.enforce_revocations().await;
                if cursor == 0 || reported_error {
                    reporter.registration(Some(Ok(label.clone())));
                    reported_error = false;
                }
                cursor = reply.cursor;
                backoff = Duration::from_secs(2);
            }
            Err(e) if e.downcast_ref::<NotSignedIn>().is_some() => {
                // Unregistered (or registered again elsewhere): stop using this registration.
                reporter.event(format!("The server no longer knows this computer ({e:#})"), true);
                reporter.registration(Some(Err("unregistered on the server".into())));
                let _ = Registration::clear();
                return;
            }
            Err(e) => {
                requeue(&server, req);
                // Server unreachable: tickets already issued keep working (up to 12 h).
                if !reported_error {
                    reporter.event(format!("Cannot reach the rdesk server: {e:#}"), true);
                    reporter.registration(Some(Err(format!("server unreachable: {e:#}"))));
                    reported_error = true;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}
