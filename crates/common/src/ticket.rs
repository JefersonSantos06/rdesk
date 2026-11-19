//! The host's side of access tickets: is this ticket from our server, for this machine,
//! for the endpoint on the other end of the connection, still valid and not revoked?

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::{Result, bail};
use rdesk_proto::Access;
use rdesk_proto::ticket::{CLOCK_SKEW_SECS, SignedTicket, TICKET_MAX_SECS, TicketClaims};

/// Who a ticket let in (kept with the session for revocations and display).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketUser {
    pub user_id: i64,
    pub email: String,
    pub name: String,
    pub issued_at: i64,
    pub expires_at: i64,
    /// What the ticket allows.
    pub access: Access,
}

impl TicketUser {
    pub fn label(&self) -> String {
        if self.name.is_empty() { self.email.clone() } else { format!("{} <{}>", self.name, self.email) }
    }
}

/// `user_id` → tickets issued at or before this time are void.
pub type Revocations = Arc<RwLock<HashMap<i64, i64>>>;

/// The server's ticket-signing keys: the current one, and the previous one for a while
/// after a rotation (updated by the sync).
pub type ServerKeys = Arc<RwLock<Vec<[u8; 32]>>>;

#[derive(Clone)]
pub struct TicketPolicy {
    pub server_keys: ServerKeys,
    /// This machine's ID.
    pub machine: [u8; 32],
    pub revoked: Revocations,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl TicketPolicy {
    /// Check a ticket presented by the endpoint `client`.
    pub fn verify(&self, bytes: &[u8], client: &[u8; 32]) -> Result<TicketUser> {
        let Ok(t) = SignedTicket::from_bytes(bytes) else { bail!("malformed ticket") };
        let sig = iroh::Signature::from_bytes(&t.signature);
        let msg = SignedTicket::signed_message(&t.claims);
        let signed = self
            .server_keys
            .read()
            .unwrap()
            .iter()
            .any(|k| iroh::PublicKey::from_bytes(k).is_ok_and(|key| key.verify(&msg, &sig).is_ok()));
        if !signed {
            bail!("the ticket was not signed by this computer's rdesk server");
        }
        let Ok(c) = TicketClaims::from_bytes(&t.claims) else { bail!("malformed ticket") };
        if c.machine != self.machine {
            bail!("the ticket is for another computer");
        }
        if &c.client != client {
            bail!("the ticket was issued to another connection");
        }
        let t = now();
        if c.expires_at < t - CLOCK_SKEW_SECS || c.issued_at > t + CLOCK_SKEW_SECS {
            bail!("the ticket expired: ask the server for a new one");
        }
        if c.expires_at - c.issued_at > TICKET_MAX_SECS {
            bail!("the ticket lasts too long");
        }
        let user = TicketUser {
            user_id: c.user_id,
            email: c.user_email,
            name: c.user_name,
            issued_at: c.issued_at,
            expires_at: c.expires_at,
            access: Access::from_text(&c.role),
        };
        if user.access.is_none() {
            bail!("the ticket allows nothing");
        }
        if self.is_revoked(&user) {
            bail!("your access to this computer was removed");
        }
        Ok(user)
    }

    /// Revoked, or expired since (a reconnect must not outlive the ticket).
    pub fn is_revoked(&self, u: &TicketUser) -> bool {
        let revoked = self.revoked.read().unwrap().get(&u.user_id).is_some_and(|before| u.issued_at <= *before);
        revoked || u.expires_at < now() - CLOCK_SKEW_SECS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(server: &iroh::SecretKey, machine: [u8; 32], client: [u8; 32], issued: i64, user_id: i64) -> Vec<u8> {
        issue_with(server, machine, client, issued, user_id, "screen,input,files")
    }

    fn issue_with(
        server: &iroh::SecretKey,
        machine: [u8; 32],
        client: [u8; 32],
        issued: i64,
        user_id: i64,
        role: &str,
    ) -> Vec<u8> {
        let claims = TicketClaims {
            ticket_id: [0; 16],
            machine,
            client,
            user_id,
            user_email: "ana@example.com".into(),
            user_name: "Ana".into(),
            role: role.into(),
            issued_at: issued,
            expires_at: issued + 3600,
        }
        .to_bytes();
        let sig = server.sign(&SignedTicket::signed_message(&claims));
        SignedTicket { claims, signature: sig.to_bytes() }.to_bytes()
    }

    #[test]
    fn access_and_rotated_keys() {
        let (old, new) = (iroh::SecretKey::generate(), iroh::SecretKey::generate());
        let (machine, client) = ([1u8; 32], [2u8; 32]);
        let p = TicketPolicy {
            server_keys: Arc::new(RwLock::new(vec![*new.public().as_bytes(), *old.public().as_bytes()])),
            machine,
            revoked: Default::default(),
        };
        let t = now();
        assert_eq!(p.verify(&issue_with(&new, machine, client, t, 1, "screen"), &client).unwrap().access, Access::VIEW);
        assert_eq!(p.verify(&issue_with(&old, machine, client, t, 1, "files"), &client).unwrap().access, Access::FILES);
        assert_eq!(
            p.verify(&issue_with(&new, machine, client, t, 1, "control"), &client).unwrap().access,
            Access::FULL
        );
        assert!(p.verify(&issue_with(&new, machine, client, t, 1, ""), &client).is_err(), "nothing allowed");
        // After the grace period only the new key remains.
        p.server_keys.write().unwrap().truncate(1);
        assert!(p.verify(&issue(&old, machine, client, t, 1), &client).is_err());
    }

    #[test]
    fn checks() {
        let server = iroh::SecretKey::generate();
        let other = iroh::SecretKey::generate();
        let (machine, client) = ([1u8; 32], [2u8; 32]);
        let p = TicketPolicy {
            server_keys: Arc::new(RwLock::new(vec![*server.public().as_bytes()])),
            machine,
            revoked: Default::default(),
        };
        let t = now();
        let ok = issue(&server, machine, client, t, 7);
        assert_eq!(p.verify(&ok, &client).unwrap().email, "ana@example.com");
        assert!(p.verify(&ok, &[3; 32]).is_err(), "bound to the client endpoint");
        assert!(p.verify(&issue(&server, [9; 32], client, t, 7), &client).is_err(), "bound to the machine");
        assert!(p.verify(&issue(&other, machine, client, t, 7), &client).is_err(), "signed by our server only");
        assert!(p.verify(&issue(&server, machine, client, t - 7200, 7), &client).is_err(), "expired");
        let mut tampered = ok.clone();
        let n = tampered.len();
        tampered[n / 2] ^= 1;
        assert!(p.verify(&tampered, &client).is_err());

        p.revoked.write().unwrap().insert(7, t);
        assert!(p.verify(&ok, &client).is_err(), "revoked");
        assert!(p.verify(&issue(&server, machine, client, t + 1, 7), &client).is_ok(), "a newer ticket works");
        assert!(p.verify(&issue(&server, machine, client, t, 8), &client).is_ok(), "other users unaffected");
    }
}
