//! Keeping a registered computer in step with its server: report addresses and whether
//! it is in use, and receive revocations (who lost access) as soon as they happen, through
//! a long poll authenticated with the machine's own token.

use std::sync::Arc;
use std::time::Duration;

use rdesk_common::account::{NotSignedIn, Registration};
use rdesk_common::transport::Endpoint;
use rdesk_common::{t, tf};
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
    let label = tf!("{} on {}", "{} em {}", reg.name, reg.server_name);
    let mut cursor = 0;
    let mut backoff = Duration::from_secs(2);
    let mut reported_error = false;
    // The newest version offered so far (the server answers at once only for a newer one).
    let mut update_seen = String::new();
    let mut macs = Vec::new();
    loop {
        if macs.is_empty() || cursor == 0 {
            macs = tokio::task::spawn_blocking(crate::wake::local_macs).await.unwrap_or_default();
        }
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
            update_seen: update_seen.clone(),
            macs: macs.clone(),
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
                        reporter.event(
                            t!(
                                "The server's ticket key changed (rotation)",
                                "A chave de tickets do servidor mudou (rotação)"
                            ),
                            false,
                        );
                    }
                }
                server.enforce_revocations().await;
                for w in reply.wake {
                    let sent = tokio::task::spawn_blocking(move || crate::wake::send(&w)).await.unwrap_or(0);
                    reporter.event(
                        tf!(
                            "Sent Wake-on-LAN packets for a computer on this network ({sent})",
                            "Pacotes Wake-on-LAN enviados para um computador desta rede ({sent})"
                        ),
                        false,
                    );
                }
                if let Some(offer) = reply.update
                    && offer.version != update_seen
                {
                    update_seen = offer.version.clone();
                    crate::update::offered(offer, &server, &reporter);
                }
                if cursor == 0 || reported_error {
                    reporter.registration(Some(Ok(label.clone())));
                    reported_error = false;
                }
                cursor = reply.cursor;
                backoff = Duration::from_secs(2);
            }
            Err(e) if e.downcast_ref::<NotSignedIn>().is_some() => {
                // Unregistered (or registered again elsewhere): stop using this registration.
                reporter.event(
                    tf!(
                        "The server no longer knows this computer ({e:#})",
                        "O servidor não conhece mais este computador ({e:#})"
                    ),
                    true,
                );
                reporter.registration(Some(Err(t!("unregistered on the server", "descadastrado no servidor").into())));
                let _ = Registration::clear();
                return;
            }
            Err(e) => {
                requeue(&server, req);
                // Server unreachable: tickets already issued keep working (up to 12 h).
                if !reported_error {
                    reporter.event(
                        tf!("Cannot reach the rdesk server: {e:#}", "Não foi possível acessar o servidor rdesk: {e:#}"),
                        true,
                    );
                    reporter.registration(Some(Err(tf!("server unreachable: {e:#}", "servidor inacessível: {e:#}"))));
                    reported_error = true;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}
