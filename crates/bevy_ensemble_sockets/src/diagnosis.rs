//! Why a connection ended up on the relay.
//!
//! "Relayed" is a conclusion, and on its own it says nothing about what to fix. ICE reaches it by
//! trying every pair of candidates the two sides offered and watching them fail, and that record
//! — who offered which address, which checks were sent, which were answered — is the whole
//! explanation. It exists only in the connection's stats, for as long as the connection does.
//!
//! So when a connection settles on the relay, the backend reads the stats into these
//! platform-neutral shapes and [`describe`] writes them out, with a verdict on top. The two sides
//! of a star link see the same pairs from opposite ends, so the host's report alone names the
//! machine at fault.

use std::fmt::Write;

/// One candidate: an address a side offered.
pub(crate) struct Candidate {
    pub id: String,
    /// `host`, `srflx`, `prflx` or `relay`.
    pub kind: String,
    pub address: String,
    pub port: u16,
    pub protocol: String,
}

/// One candidate pair and what the connectivity checks on it did.
pub(crate) struct Pair {
    pub local_id: String,
    pub remote_id: String,
    pub state: String,
    pub nominated: bool,
    /// The connectivity-check counters, where the backend keeps them. Browsers do; webrtc-rs
    /// reports only a pair's state, and a zero there would read as "nothing was sent".
    pub checks: Option<Checks>,
}

/// STUN connectivity checks on one pair, from this side's point of view.
#[derive(Clone, Copy)]
pub(crate) struct Checks {
    pub sent: u64,
    pub answered: u64,
    pub received: u64,
    pub answered_back: u64,
}

/// The report: a verdict, then every candidate and every pair.
pub(crate) fn describe(
    peer: u128,
    selected: Option<(&str, &str)>,
    local: &[Candidate],
    remote: &[Candidate],
    pairs: &[Pair],
) -> String {
    let find = |id: &str| {
        local
            .iter()
            .chain(remote.iter())
            .find(|candidate| candidate.id == id)
    };
    let label = |id: &str| match find(id) {
        Some(c) => format!("{} {}:{}", c.kind, c.address, c.port),
        None => format!("? ({id})"),
    };

    let mut out = String::new();
    let _ = writeln!(out, "relayed connection to peer {peer:#x}");
    for line in verdict(local, remote, pairs, find) {
        let _ = writeln!(out, "  verdict: {line}");
    }
    if let Some((l, r)) = selected {
        let _ = writeln!(out, "  selected: {} <-> {}", label(l), label(r));
    }
    for (side, candidates) in [("local", local), ("remote", remote)] {
        let _ = writeln!(out, "  {side} candidates ({}):", candidates.len());
        for c in candidates {
            let _ = writeln!(
                out,
                "    {:5} {}:{} {}",
                c.kind, c.address, c.port, c.protocol
            );
        }
    }
    let _ = writeln!(out, "  pairs ({}):", pairs.len());
    for p in pairs {
        let checks = p.checks.map_or(String::new(), |c| {
            format!(
                ", checks sent {} answered {}, received {} answered {}",
                c.sent, c.answered, c.received, c.answered_back
            )
        });
        let _ = writeln!(
            out,
            "    {} -> {}: {}{}{checks}",
            label(&p.local_id),
            label(&p.remote_id),
            p.state,
            if p.nominated { " (nominated)" } else { "" },
        );
    }
    out
}

/// What the record most likely means, in the order it would be fixed. Hedged, because a check
/// that got no answer cannot say which end dropped it; the raw record below it is the evidence.
fn verdict<'a>(
    local: &'a [Candidate],
    remote: &'a [Candidate],
    pairs: &[Pair],
    find: impl Fn(&str) -> Option<&'a Candidate>,
) -> Vec<String> {
    let host = |c: &&Candidate| c.kind == "host";
    let local_hosts: Vec<&Candidate> = local.iter().filter(host).collect();
    let remote_hosts: Vec<&Candidate> = remote.iter().filter(host).collect();
    let mut lines = Vec::new();

    if local_hosts.is_empty() {
        lines.push("this machine offered no LAN address, so no direct pair was possible".into());
    }
    if remote_hosts.is_empty() {
        lines.push(
            "the other side offered no LAN address (a browser with LAN addresses hidden, or \
             nothing but a relay configured), so no direct pair was possible"
                .into(),
        );
    } else if remote_hosts.iter().all(|c| c.address.ends_with(".local")) {
        lines.push(
            "the other side's LAN address arrived as an mDNS name (*.local), which a browser uses \
             to hide it; it only works if multicast DNS reaches between the two machines"
                .into(),
        );
    }

    let lan: Vec<&Pair> = pairs
        .iter()
        .filter(|p| {
            find(&p.local_id).is_some_and(|c| c.kind == "host")
                && find(&p.remote_id).is_some_and(|c| c.kind == "host")
        })
        .collect();
    if !lan.is_empty() && !lan.iter().any(|p| p.state == "succeeded") {
        let counted: Vec<Checks> = lan.iter().filter_map(|p| p.checks).collect();
        let pending = lan
            .iter()
            .filter(|p| matches!(p.state.as_str(), "in-progress" | "waiting" | "frozen"))
            .count();
        lines.push(if !counted.is_empty() {
            let sent: u64 = counted.iter().map(|c| c.sent).sum();
            let heard: u64 = counted.iter().map(|c| c.received).sum();
            if heard > 0 {
                format!(
                    "LAN checks from the other side arrived ({heard}) but none of ours ({sent}) \
                     were answered: most likely the other machine's firewall drops incoming UDP"
                )
            } else if sent > 0 {
                format!(
                    "no LAN check got through in either direction ({sent} sent, none heard): a \
                     firewall on either machine, or a network that keeps its devices apart"
                )
            } else {
                "LAN pairs existed but were never checked".into()
            }
        } else if pending > 0 {
            format!(
                "the relay was chosen while {pending} of {} LAN pairs were still being checked: \
                 they may have been slow rather than blocked",
                lan.len()
            )
        } else {
            format!(
                "every LAN pair ({}) failed its checks, although both sides offered LAN \
                 addresses: most likely a firewall on one of the two machines dropping incoming \
                 UDP (a browser peer's report counts the checks, and says which)",
                lan.len()
            )
        });
    }

    let public_failed = pairs.iter().any(|p| {
        find(&p.local_id).is_some_and(|c| c.kind == "srflx")
            && find(&p.remote_id).is_some_and(|c| c.kind == "srflx")
            && p.state != "succeeded"
    });
    if public_failed {
        lines.push(
            "public-address pairs failed too: when both machines share a public IP, the router \
             would have to hairpin, and many do not"
                .into(),
        );
    }

    if lines.is_empty() {
        lines.push("no single cause stands out; the record below is everything ICE tried".into());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, kind: &str, address: &str) -> Candidate {
        Candidate {
            id: id.into(),
            kind: kind.into(),
            address: address.into(),
            port: 5000,
            protocol: "udp".into(),
        }
    }

    fn pair(local: &str, remote: &str, sent: u64, answered: u64, heard: u64) -> Pair {
        Pair {
            local_id: local.into(),
            remote_id: remote.into(),
            state: if answered > 0 { "succeeded" } else { "failed" }.into(),
            nominated: false,
            checks: Some(Checks {
                sent,
                answered,
                received: heard,
                answered_back: heard,
            }),
        }
    }

    /// webrtc-rs counts nothing, so the verdict has only the pairs' states to go on.
    #[test]
    fn without_counters_failed_lan_pairs_still_point_at_a_firewall() {
        let local = [candidate("l1", "host", "10.0.0.2")];
        let remote = [candidate("r1", "host", "10.0.0.3")];
        let mut failed = pair("l1", "r1", 0, 0, 0);
        failed.checks = None;
        let report = describe(7, None, &local, &remote, &[failed]);
        assert!(report.contains("every LAN pair (1) failed"), "{report}");
        assert!(!report.contains("checks sent"), "{report}");
    }

    #[test]
    fn a_lan_pair_that_worked_is_not_blamed() {
        let local = [candidate("l1", "host", "10.0.0.2")];
        let remote = [candidate("r1", "host", "10.0.0.3")];
        let mut worked = pair("l1", "r1", 0, 0, 0);
        worked.state = "succeeded".into();
        worked.checks = None;
        let report = describe(7, None, &local, &remote, &[worked]);
        assert!(report.contains("no single cause stands out"), "{report}");
    }

    #[test]
    fn a_firewall_on_the_other_side_is_named() {
        let local = [
            candidate("l1", "host", "10.0.0.2"),
            candidate("l2", "relay", "1.2.3.4"),
        ];
        let remote = [
            candidate("r1", "host", "10.0.0.3"),
            candidate("r2", "relay", "1.2.3.4"),
        ];
        let pairs = [pair("l1", "r1", 8, 0, 5), pair("l2", "r2", 3, 3, 3)];
        let report = describe(7, Some(("l2", "r2")), &local, &remote, &pairs);
        assert!(report.contains("other machine's firewall"), "{report}");
        assert!(report.contains("selected: relay 1.2.3.4:5000"), "{report}");
        assert!(
            report.contains("host 10.0.0.2:5000 -> host 10.0.0.3:5000: failed"),
            "{report}"
        );
    }

    #[test]
    fn a_hidden_lan_address_is_named() {
        let local = [candidate("l1", "host", "10.0.0.2")];
        let remote = [candidate("r1", "host", "8f2c.local")];
        let report = describe(7, None, &local, &remote, &[]);
        assert!(report.contains("mDNS name"), "{report}");
    }

    #[test]
    fn no_lan_address_at_all_is_named() {
        let local = [candidate("l1", "host", "10.0.0.2")];
        let remote = [candidate("r2", "relay", "1.2.3.4")];
        let report = describe(7, None, &local, &remote, &[]);
        assert!(
            report.contains("the other side offered no LAN address"),
            "{report}"
        );
    }
}
