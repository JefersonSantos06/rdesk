//! A newer rdesk released by the server's admin. On Windows the service's host installs it
//! by itself (`win::update`), and so does the app on a Mac (`mac_update`); on Linux the
//! host only says it exists (the `rdesk-update` timer installs packages).

use std::sync::Arc;

use rdesk_common::{t, tf};
use rdesk_proto::api::UpdateOffer;
use rdesk_proto::release::parse_version;

use crate::control::Reporter;
use crate::server::Server;

/// Is `version` newer than this build?
pub fn is_newer(version: &str) -> bool {
    matches!((parse_version(version), parse_version(env!("CARGO_PKG_VERSION"))), (Some(v), Some(me)) if v > me)
}

pub(crate) fn offered(offer: UpdateOffer, server: &Arc<Server>, reporter: &Reporter) {
    if !is_newer(&offer.version) {
        return;
    }
    #[cfg(windows)]
    if server.agent {
        tokio::spawn(crate::win::update::install(offer, server.clone(), reporter.clone()));
        return;
    }
    #[cfg(target_os = "macos")]
    if crate::mac_update::bundle().is_some() {
        tokio::spawn(crate::mac_update::install(offer, server.clone(), reporter.clone()));
        return;
    }
    let _ = server;
    let how = if cfg!(windows) {
        t!(
            "install the rdesk service (This computer) to get updates by itself",
            "instale o serviço do rdesk (Este computador) para receber atualizações sozinho"
        )
    } else if cfg!(target_os = "linux") {
        t!("the rdesk-update timer installs it, if set up", "o timer rdesk-update o instala, se estiver configurado")
    } else {
        t!("download it from the release page", "baixe-o na página de versões")
    };
    reporter.event(tf!("rdesk {} is available: {how}", "rdesk {} está disponível: {how}", offer.version), false);
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn newer() {
        assert!(is_newer("999.0.0"));
        assert!(!is_newer("0.0.1"));
        assert!(!is_newer(env!("CARGO_PKG_VERSION")));
        assert!(!is_newer("garbage"));
    }
}
