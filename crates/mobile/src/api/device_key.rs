//! The phone's own key for computers with the extra lock: an ECDSA P-256 key the app makes
//! in the phone's secure hardware (Android Keystore, Apple's Secure Enclave). It never
//! leaves it; every login asks the phone's lock (fingerprint, Face ID) to sign.

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use flutter_rust_bridge::{DartFnFuture, frb};
use rdesk_common::auth::{ExternalSigner, SignFuture};
use ssh_key::PublicKey;

/// The OpenSSH line for a hardware public key, to authorize on a computer
/// (`rdesk host add-key "…"`, or *This computer → Security → SSH keys*).
/// `public_key`: the uncompressed point (65 bytes, as iOS exports it) or the X.509
/// SubjectPublicKeyInfo (91 bytes, as Android does).
pub fn device_key_line(public_key: Vec<u8>, comment: String) -> Result<String> {
    let mut k = parse(&public_key)?;
    k.set_comment(comment);
    Ok(k.to_openssh()?)
}

/// `SHA256:…`, as `rdesk agent-keys` and the computer's key list show it.
pub fn device_key_fingerprint(public_key: Vec<u8>) -> Result<String> {
    Ok(parse(&public_key)?.fingerprint(ssh_key::HashAlg::Sha256).to_string())
}

#[frb(ignore)]
pub(crate) fn parse(bytes: &[u8]) -> Result<PublicKey> {
    // A P-256 SubjectPublicKeyInfo ends with the uncompressed point.
    let point = match bytes.len() {
        65 => bytes,
        91 => &bytes[26..],
        n => bail!("not a P-256 public key ({n} bytes)"),
    };
    let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(point).map_err(|e| anyhow!("not a P-256 public key: {e}"))?;
    Ok(PublicKey::from(ssh_key::public::EcdsaPublicKey::from(vk)))
}

/// Signs through the app: the Dart side asks the hardware (and the user) for each signature.
#[frb(ignore)]
pub(crate) struct DartSigner {
    public: PublicKey,
    sign: Arc<dyn Fn(Vec<u8>) -> DartFnFuture<Vec<u8>> + Send + Sync>,
}

impl DartSigner {
    pub(crate) fn new(
        public_key: &[u8],
        sign: impl Fn(Vec<u8>) -> DartFnFuture<Vec<u8>> + Send + Sync + 'static,
    ) -> Result<Self> {
        Ok(Self { public: parse(public_key)?, sign: Arc::new(sign) })
    }
}

impl ExternalSigner for DartSigner {
    fn public_key(&self) -> &PublicKey {
        &self.public
    }

    fn sign_der(&self, data: Vec<u8>) -> SignFuture {
        let fut = (self.sign)(data);
        Box::pin(async move {
            let der = fut.await;
            if der.is_empty() {
                bail!(rdesk_common::t!("The phone did not sign (cancelled?).", "O celular não assinou (cancelado?)."));
            }
            Ok(der)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_export_formats_give_the_same_key() {
        let sk = p256::ecdsa::SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let point = sk.verifying_key().to_encoded_point(false);
        let raw = point.as_bytes().to_vec();
        // SubjectPublicKeyInfo: the fixed DER prefix for id-ecPublicKey + prime256v1, then the point.
        let mut spki = hex_prefix();
        spki.extend_from_slice(&raw);
        assert_eq!(spki.len(), 91);
        let a = device_key_line(raw, "phone".into()).unwrap();
        let b = device_key_line(spki, "phone".into()).unwrap();
        assert_eq!(a, b);
        assert!(a.starts_with("ecdsa-sha2-nistp256 ") && a.ends_with(" phone"), "{a}");
        assert!(device_key_line(vec![4; 30], String::new()).is_err());
    }

    fn hex_prefix() -> Vec<u8> {
        let h = "3059301306072a8648ce3d020106082a8648ce3d030107034200";
        (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect()
    }
}
