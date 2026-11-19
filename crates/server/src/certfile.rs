//! A certificate from PEM files, for when a reverse proxy does the HTTPS (`tls = "none"`)
//! but the relay's QUIC address discovery still needs TLS: point `tls_cert` / `tls_key`
//! at the proxy's Let's Encrypt files. They are read again when they change (renewals).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use rustls::crypto::ring::sign::any_supported_type;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

/// How often the files are checked for a renewal.
const CHECK_EVERY: Duration = Duration::from_secs(300);

#[derive(Debug)]
pub struct FileCert {
    cert: PathBuf,
    key: PathBuf,
    state: Mutex<Loaded>,
}

#[derive(Debug)]
struct Loaded {
    key: Arc<CertifiedKey>,
    modified: Option<SystemTime>,
    checked: Instant,
}

fn modified(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn load(cert: &Path, key: &Path) -> Result<Arc<CertifiedKey>> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading {}", cert.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing {}", cert.display()))?;
    if chain.is_empty() {
        return Err(anyhow!("{}: no certificate", cert.display()));
    }
    let key = PrivateKeyDer::from_pem_file(key).with_context(|| format!("reading {}", key.display()))?;
    let signer = any_supported_type(&key).map_err(|e| anyhow!("unsupported private key: {e}"))?;
    Ok(Arc::new(CertifiedKey::new(chain, signer)))
}

impl FileCert {
    pub fn new(cert: &Path, key: &Path) -> Result<Self> {
        let loaded = Loaded { key: load(cert, key)?, modified: modified(cert), checked: Instant::now() };
        Ok(Self { cert: cert.to_path_buf(), key: key.to_path_buf(), state: Mutex::new(loaded) })
    }
}

impl ResolvesServerCert for FileCert {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let mut st = self.state.lock().unwrap();
        if st.checked.elapsed() >= CHECK_EVERY {
            st.checked = Instant::now();
            let now = modified(&self.cert);
            if now != st.modified {
                match load(&self.cert, &self.key) {
                    Ok(k) => {
                        tracing::info!("relay QUIC: certificate reloaded from {}", self.cert.display());
                        st.key = k;
                        st.modified = now;
                    }
                    // Mid-renewal (one file written, not the other): keep the old one.
                    Err(e) => tracing::warn!("relay QUIC: {e:#}"),
                }
            }
        }
        Some(st.key.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_files_are_reported() {
        let e = FileCert::new(Path::new("/nope/cert.pem"), Path::new("/nope/key.pem")).unwrap_err();
        assert!(format!("{e:#}").contains("cert.pem"));
    }
}
