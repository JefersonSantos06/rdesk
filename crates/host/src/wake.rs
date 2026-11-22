//! Wake-on-LAN: this computer reports its network cards to the server, and wakes others on
//! its network when the server asks (someone clicked "Turn on" for a computer that is off;
//! only a computer that is on, on the same network, can reach it). Like TeamViewer's and
//! AnyDesk's "wake through another device".

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

use rdesk_proto::api::WakeRequest;

/// This computer's network cards that have an address on a local network
/// ("aa:bb:cc:dd:ee:ff").
pub fn local_macs() -> Vec<String> {
    let mut out: Vec<String> = netdev::get_interfaces()
        .into_iter()
        .filter(|i| i.is_up() && !i.is_loopback() && i.ipv4.iter().any(|n| n.addr().is_private()))
        .filter_map(|i| i.mac_addr.map(|m| m.octets()))
        .filter(|m| *m != [0; 6])
        .map(|m| format_mac(&m))
        .collect();
    out.sort();
    out.dedup();
    out.truncate(8);
    out
}

pub fn format_mac(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<u8> = s.split([':', '-']).map(|p| u8::from_str_radix(p, 16)).collect::<Result<_, _>>().ok()?;
    parts.try_into().ok()
}

/// The magic packet: six 0xFF, then the address sixteen times.
pub fn magic_packet(mac: &[u8; 6]) -> [u8; 102] {
    let mut p = [0xFFu8; 102];
    for i in 0..16 {
        p[6 + i * 6..12 + i * 6].copy_from_slice(mac);
    }
    p
}

/// Where to send: everyone on this network, and the broadcast of each /24 the sleeping
/// computer had an address in.
fn targets(addrs: &[String]) -> Vec<SocketAddr> {
    let mut out = vec![SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, 9))];
    for a in addrs {
        let ip = a.parse::<SocketAddr>().map(|s| s.ip()).or_else(|_| a.parse());
        if let Ok(std::net::IpAddr::V4(v4)) = ip
            && v4.is_private()
        {
            let o = v4.octets();
            let b = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(o[0], o[1], o[2], 255), 9));
            if !out.contains(&b) {
                out.push(b);
            }
        }
    }
    out
}

/// Send the packets for `req` (a few times: they are tiny and UDP may drop one).
pub fn send(req: &WakeRequest) -> usize {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return 0 };
    let _ = sock.set_broadcast(true);
    let mut sent = 0;
    for mac in req.macs.iter().filter_map(|m| parse_mac(m)) {
        let packet = magic_packet(&mac);
        for t in targets(&req.addrs) {
            for port in [9, 7] {
                let t = SocketAddr::new(t.ip(), port);
                for _ in 0..3 {
                    if sock.send_to(&packet, t).is_ok() {
                        sent += 1;
                    }
                }
            }
        }
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_and_addresses() {
        let mac = parse_mac("AA:bb:cc:01:02:03").unwrap();
        assert_eq!(format_mac(&mac), "aa:bb:cc:01:02:03");
        assert!(parse_mac("aa:bb:cc").is_none());
        let p = magic_packet(&mac);
        assert_eq!(&p[..6], &[0xFF; 6]);
        assert_eq!(&p[96..], &mac);
        let t = targets(&["192.168.0.20:7420".into(), "8.8.8.8:1".into(), "10.1.2.3".into()]);
        assert_eq!(
            t,
            vec![
                "255.255.255.255:9".parse().unwrap(),
                "192.168.0.255:9".parse().unwrap(),
                "10.1.2.255:9".parse().unwrap()
            ]
        );
    }
}
