//! The networks this computer's interfaces are on. Direct access ("local network") also
//! counts addresses on those networks: two computers in one house often talk over public
//! IPv6 addresses of the same /64. A connection can not fake such an address and still
//! log in: the QUIC handshake needs the replies sent to it.

use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use ipnet::IpNet;

#[derive(Clone, Default)]
pub struct LocalNets(Arc<RwLock<Vec<IpNet>>>);

impl LocalNets {
    /// Keep the list current (interfaces come and go) while the host runs.
    pub fn watch() -> Self {
        let nets = Self::default();
        let me = nets.clone();
        tokio::spawn(async move {
            loop {
                me.refresh().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        nets
    }

    async fn refresh(&self) {
        let state = netwatch::interfaces::State::new().await;
        let mut nets = Vec::new();
        for iface in state.interfaces.values().filter(|i| i.is_up()) {
            for n in iface.addrs() {
                let net = match n {
                    netwatch::interfaces::IpNet::V4(v4) => IpNet::V4(v4),
                    netwatch::interfaces::IpNet::V6 { net, .. } => IpNet::V6(net),
                };
                if specific(&net) {
                    nets.push(net.trunc());
                }
            }
        }
        *self.0.write().unwrap() = nets;
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.0.read().unwrap().iter().any(|n| n.contains(&ip))
    }

    #[cfg(test)]
    fn with(nets: &[&str]) -> Self {
        Self(Arc::new(RwLock::new(nets.iter().map(|n| n.parse().unwrap()).collect())))
    }
}

/// A real local network, not something as wide as "the internet" (a /0 default route, or a
/// provider's whole block).
fn specific(net: &IpNet) -> bool {
    match net {
        IpNet::V4(n) => n.prefix_len() >= 8,
        IpNet::V6(n) => n.prefix_len() >= 48,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_network() {
        let nets = LocalNets::with(&["2804:1530:6a2:f000::/64", "192.168.0.0/24"]);
        assert!(nets.contains("2804:1530:6a2:f000:2e3e:25fd:5e3f:6c8a".parse().unwrap()));
        assert!(nets.contains("192.168.0.77".parse().unwrap()));
        assert!(nets.contains("::ffff:192.168.0.77".parse().unwrap()));
        assert!(!nets.contains("2804:1530:6a2:f001::1".parse().unwrap()), "the neighbour's /64");
        assert!(!nets.contains("8.8.8.8".parse().unwrap()));
        assert!(!specific(&"0.0.0.0/0".parse().unwrap()));
        assert!(!specific(&"2804::/16".parse().unwrap()));
        assert!(specific(&"2804:1530:6a2:f000::/64".parse().unwrap()));
    }
}
