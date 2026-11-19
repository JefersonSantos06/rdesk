//! Access tickets: the server's signed statement that a user may control a machine from
//! one specific endpoint until a given time. The host checks them on its own with the
//! server's public key (kept when the machine was registered).
//!
//! Wire format: postcard of [`SignedTicket`]; the signature (Ed25519, by the server key)
//! covers [`TICKET_DOMAIN`] followed by the postcard bytes of the [`TicketClaims`].

use serde::{Deserialize, Serialize};

pub const TICKET_DOMAIN: &[u8] = b"rdesk-ticket-v1";
/// Tickets never last longer than this (also how long they work while the server is down).
pub const TICKET_MAX_SECS: i64 = 12 * 3600;
/// Clock difference tolerated between server and host.
pub const CLOCK_SKEW_SECS: i64 = 300;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketClaims {
    pub ticket_id: [u8; 16],
    /// The machine's endpoint ID (host ID).
    pub machine: [u8; 32],
    /// The endpoint ID the client connects from.
    pub client: [u8; 32],
    pub user_id: i64,
    pub user_email: String,
    pub user_name: String,
    /// What the user may do; for now always "control".
    pub role: String,
    pub issued_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedTicket {
    pub claims: Vec<u8>,
    #[serde(with = "serde_sig")]
    pub signature: [u8; 64],
}

impl TicketClaims {
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("ticket claims serialize")
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(b)
    }
}

impl SignedTicket {
    /// What the signature covers.
    pub fn signed_message(claims: &[u8]) -> Vec<u8> {
        let mut m = TICKET_DOMAIN.to_vec();
        m.extend_from_slice(claims);
        m
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("ticket serializes")
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(b)
    }
}

/// serde has no impls for `[u8; 64]`; store it as bytes.
mod serde_sig {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        let v: Vec<u8> = Deserialize::deserialize(d)?;
        v.try_into().map_err(|_| serde::de::Error::custom("signature must be 64 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let c = TicketClaims {
            ticket_id: [1; 16],
            machine: [2; 32],
            client: [3; 32],
            user_id: 7,
            user_email: "a@b.c".into(),
            user_name: "A".into(),
            role: "control".into(),
            issued_at: 10,
            expires_at: 20,
        };
        let t = SignedTicket { claims: c.to_bytes(), signature: [9; 64] };
        let back = SignedTicket::from_bytes(&t.to_bytes()).unwrap();
        assert_eq!(back, t);
        assert_eq!(TicketClaims::from_bytes(&back.claims).unwrap(), c);
    }
}
