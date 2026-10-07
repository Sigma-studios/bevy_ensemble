//! Which network interfaces the native backend gathers host candidates from.
//!
//! A machine running Docker, Podman, a hypervisor or a Kubernetes CNI has a bridge per network,
//! each with a private address that only the machine itself can reach. Offered as host
//! candidates they cannot connect to anything, and they still cost: every local address
//! multiplies the pairs to check, the remote side checks them at a fixed pace, and they share the
//! one real LAN address's priority, so it waits its turn among them. A player with 26 Compose
//! networks offered 27 host candidates to a browser host on the same LAN; the relay pair
//! succeeded first and was nominated nine times out of ten. libnice skips such interfaces by
//! default for the same reason (its `ignored-network-interface-prefixes` option: `docker`,
//! `veth`, `virbr`, `vnet`).
//!
//! Two measures, because neither covers everything:
//!
//! - [`gathers_from`] drops interfaces named like those bridges. It works with no internet route
//!   (a LAN party), but only for names on the list, and not on Windows.
//! - [`promote_default_route`] ranks the host candidate on the interface that carries the
//!   default route above the other host candidates, so the remote side checks it first. That is
//!   the address browsers offer alone (RFC 8828 mode 2,
//!   <https://www.rfc-editor.org/rfc/rfc8828#section-5.2>); offering the rest after it, rather
//!   than not at all, keeps a second LAN or a VPN working at the cost of checks that come later.
//!   It needs no names, but it needs a default route.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

/// Name prefixes of host-internal virtual networks: container bridges and veth pairs (Docker,
/// Podman, Kubernetes CNIs) and hypervisor host-only / NAT networks. Matched case-insensitively.
///
/// Taken from ant-p2p's `VIRTUAL_INTERFACE_PREFIXES` (Apache-2.0), which filters the same
/// interfaces out of its same-LAN dialing:
/// <https://github.com/freedom-hq/ant/blob/bb5fa97fc7b5624c26c7d986508dfb83d843d310/crates/ant-p2p/src/underlay.rs#L380-L397>
///
/// VPN tunnels (`tun*`, `utun*`, `wg*`, `tailscale*`) are deliberately absent: those do reach
/// other machines. So are plain `br0` / `bridge0`, which are commonly the real LAN bridged for
/// VMs. ant-p2p also matches Hyper-V's internal `vEthernet (…)` switches; that is left out here
/// because webrtc-util reports every Windows interface with an empty name, so no name filter
/// can act there.
const VIRTUAL_INTERFACE_PREFIXES: &[&str] = &[
    "docker", "veth", "cni", "flannel", "cali", "cilium", "weave", "kube-", "virbr", "vboxnet",
    "vmnet", "podman", "lxcbr", "lxdbr",
];

/// Should ICE gather host candidates on the interface called `name`? The webrtc-rs
/// `SettingEngine::set_interface_filter` callback: `false` drops the interface.
pub(crate) fn gathers_from(name: &str) -> bool {
    !is_virtual_interface(name)
}

fn is_virtual_interface(name: &str) -> bool {
    let has_prefix = |p: &str| {
        name.get(..p.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(p))
    };
    VIRTUAL_INTERFACE_PREFIXES.iter().any(|p| has_prefix(p)) || is_docker_network_bridge(name)
}

/// Docker names a user-defined network's bridge `br-` and the first 12 hex digits of the
/// network id (`br-68148f6025cd`). A bare `br-` prefix would also catch OpenWrt's real LAN
/// bridge, `br-lan`. Same source as [`VIRTUAL_INTERFACE_PREFIXES`].
fn is_docker_network_bridge(name: &str) -> bool {
    name.strip_prefix("br-")
        .is_some_and(|id| id.len() == 12 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The local addresses the system would send from to reach the internet, one per IP family that
/// has a default route. Connecting a UDP socket only looks the route up; nothing is sent. The
/// targets are documentation addresses (RFC 5737, RFC 3849): any address outside the local
/// networks resolves to the default route, and these are guaranteed to be nobody's.
pub(crate) fn default_route_addrs() -> Vec<IpAddr> {
    let probe = |bind: IpAddr, target: IpAddr| {
        let socket = UdpSocket::bind(SocketAddr::new(bind, 0)).ok()?;
        socket.connect(SocketAddr::new(target, 9)).ok()?;
        Some(socket.local_addr().ok()?.ip())
    };
    [
        probe(
            Ipv4Addr::UNSPECIFIED.into(),
            Ipv4Addr::new(192, 0, 2, 1).into(),
        ),
        probe(
            Ipv6Addr::UNSPECIFIED.into(),
            Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).into(),
        ),
    ]
    .into_iter()
    .flatten()
    .filter(|ip| !ip.is_unspecified())
    .collect()
}

/// How far a host candidate off the default route is ranked down: one step of the local
/// preference field (RFC 8445 §5.1.2.1, priority = type preference << 24 | local preference << 8
/// | 256 - component). webrtc-rs gives every host candidate the top local preference, 65535, so
/// promoting the default route's means demoting the rest; one step keeps them above every
/// server-reflexive and relay candidate.
const DEMOTION: u32 = 1 << 8;

/// `candidate` (an SDP `candidate:` line) with its priority lowered if it is a host candidate
/// on an address other than `default_route`. Unchanged otherwise, including when
/// `default_route` is empty: with no default route there is nothing to rank first.
///
/// webrtc-rs has no setting for a candidate's priority, so the line sent to the remote side is
/// rewritten instead. The remote side orders its checks by the priority in that line; the
/// PRIORITY in our own checks still carries the original, which only matters to a remote that
/// learns a peer-reflexive candidate from them, and those are ranked by type anyway.
pub(crate) fn promote_default_route(candidate: &str, default_route: &[IpAddr]) -> String {
    let mut fields: Vec<&str> = candidate.split(' ').collect();
    // candidate:<foundation> <component> <transport> <priority> <address> <port> typ <type> ...
    let is_host = fields.get(6) == Some(&"typ") && fields.get(7) == Some(&"host");
    let address = fields.get(4).and_then(|a| a.parse::<IpAddr>().ok());
    let priority = fields.get(3).and_then(|p| p.parse::<u32>().ok());
    let (true, Some(address), Some(priority)) = (is_host, address, priority) else {
        return candidate.to_owned();
    };
    if default_route.is_empty() || default_route.contains(&address) {
        return candidate.to_owned();
    }
    let demoted = priority.saturating_sub(DEMOTION).to_string();
    fields[3] = &demoted;
    fields.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAN: &str = "candidate:1 1 udp 2130706431 10.213.18.82 60707 typ host";
    const DOCKER: &str = "candidate:2 1 udp 2130706431 172.17.0.1 49253 typ host";
    const SRFLX: &str =
        "candidate:3 1 udp 1694498815 46.193.67.57 35327 typ srflx raddr 0.0.0.0 rport 60707";

    fn lan() -> Vec<IpAddr> {
        vec!["10.213.18.82".parse().unwrap()]
    }

    #[test]
    fn the_default_route_is_checked_before_the_other_host_candidates() {
        let priority = |line: &str| line.split(' ').nth(3).unwrap().parse::<u32>().unwrap();
        let lan_line = promote_default_route(LAN, &lan());
        let docker_line = promote_default_route(DOCKER, &lan());
        assert_eq!(lan_line, LAN);
        assert_eq!(
            docker_line,
            "candidate:2 1 udp 2130706175 172.17.0.1 49253 typ host"
        );
        assert!(priority(&lan_line) > priority(&docker_line));
        assert!(priority(&docker_line) > priority(SRFLX));
    }

    #[test]
    fn only_host_candidates_are_reranked_and_only_with_a_default_route() {
        assert_eq!(promote_default_route(SRFLX, &lan()), SRFLX);
        assert_eq!(promote_default_route(DOCKER, &[]), DOCKER);
        assert_eq!(promote_default_route("garbage", &lan()), "garbage");
    }

    #[test]
    fn container_and_hypervisor_bridges_are_skipped() {
        for name in [
            "docker0",
            "br-68148f6025cd",
            "veth3a1b2c4",
            "virbr0",
            "vboxnet0",
            "vmnet8",
            "podman0",
            "cni0",
            "lxdbr0",
            "Docker0",
        ] {
            assert!(!gathers_from(name), "{name}");
        }
    }

    #[test]
    fn real_lans_and_tunnels_are_kept() {
        for name in [
            "eth0",
            "enp3s0",
            "wlan0",
            "wlp2s0",
            "en0",
            "br0",
            "br-lan",
            "bridge0",
            "wg0",
            "tun0",
            "utun3",
            "tailscale0",
            "",
        ] {
            assert!(gathers_from(name), "{name}");
        }
    }
}
