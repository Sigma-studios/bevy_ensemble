# Trust model

What a peer believes, what it refuses, and what this stack deliberately does not do.

## The host is the authority

A session is a star: one host, N clients, every packet crossing the host. The host decides the
roster, relays broadcasts, and answers for the session's state. A client trusts its host and
nobody else; a host trusts the transport's word about who sent a packet and nothing a packet says
about itself.

## What is believed

| Fact | Source | Believed by |
|---|---|---|
| Who sent a packet | the transport (WebRTC peer, Steam ID, loopback peer) | everyone |
| Who the host is | `HostUuid`, set by the backend as part of joining, before any data flows | clients |
| A message's sender inside a broadcast envelope | the host, which overwrites it with the transport sender before relaying | clients |
| A player's own data (`SyncPlayerData`) | accepted on the host only when the transport sender is that player | host |
| The roster (`SyncLobbyParticipant`, `RemoveLobbyParticipant`) | the host only | clients |
| A ping's round trip | this peer's own clock; the pong carries only a sequence number and a dwell, and the dwell is clamped into the round trip | everyone |
| The other side's protocol | its `ProtocolHandshake` (hash of sorted wire names plus version), compared at the join | both |

## What is refused, and how loudly

A message type is registered with a `MessageAuthority`. `Any` is accepted from any connected
peer. `HostOnly` is accepted on a client only from the peer named by `HostUuid`; on the host from
anyone, because the host is the one deciding what to do with it. Control types — roster, pings,
handshakes, the broadcast envelope itself — are never relayed by the broadcast path, so a client
cannot wrap one in an envelope and have the host announce it.

Refusals are counted in `RefusedPackets` by wire index. The first three of each kind are logged
at `warn!`, the rest at `debug!`: a peer that sends what it may not send does so at frame rate.

Every backend also filters below the core: the WebRTC client accepts SDP offers only from its
host and decodes packets only from it; the host decodes only from peers the signalling server
introduced; Steam accepts P2P sessions and packets only from lobby members. The signalling server
authenticates before listing, hands out random lobby ids, rate-limits, and relays signals only
between a lobby's host and one of its members.

## Liveness

A peer that stops answering pings for `PeerTimeout` (5 s) is gone: the host despawns its
`LobbyClient`, which tells everyone; a client whose host is gone ends its session with
`LobbyLeft { reason: PeerTimeout }`. The transport's own disconnect detection is not relied on
alone, because a NAT binding that expired or a tab in the background looks connected to the
transport for as long as it takes ICE to give up.

## Threats this closes

| Threat | Closed by |
|---|---|
| A client speaks as the host through the relay | host stamps the envelope with the transport sender |
| A client edits another player's data | host requires sender == player |
| A client kicks anyone or forges the roster via an envelope | control types are not relayable; roster types are `HostOnly` |
| A nested envelope makes the host fan out D×N packets | an envelope is a control type |
| A pong poisons the RTT estimate with `NaN` or a huge dwell | sequence-matched pongs, own-clock timing, clamped dwell, finite guard |
| A garbage SDP offer aborts a browser peer | negotiation errors are warnings |
| A lobby member becomes a joiner's "host" by offering first | clients accept offers only from `HostUuid` |
| Anyone on Steam joins a lobby by sending a packet | membership checks before spawning a client |
| Two builds with different registrations play a silently corrupted session | protocol handshake at the join |
| A dead or backgrounded peer stalls everyone until ICE gives up | `PeerTimeout` |

## Non-goals, stated

- **Host migration.** When the host goes, the session ends, with `LobbyLeft { HostGone }` on
  every client. The host is the authority and the relay; electing a new one means transferring
  authoritative state to a peer that has only ever held a copy of it, and this stack does not
  attempt that. A game that wants continuity rehosts: one client hosts a new lobby and the others
  join it.
- **Cheating by a client with its own inputs.** A client's input is its own to send. What the
  host makes of it is the game's simulation, not this layer's.
- **Encryption beyond the transport's.** WebRTC data channels and Steam networking are encrypted
  on the wire; the core adds none of its own.
