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

#[cfg(test)]
mod tests {
    use super::*;

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
