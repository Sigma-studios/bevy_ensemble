//! Measure a TURN relay the way the game will actually use it, so a candidate can be judged on
//! numbers instead of a hunch.
//!
//! # Why not just ping it
//!
//! An ICMP round trip to a relay's front door answers a question nobody asked. What a relayed
//! session costs is **two crossings of the server in each direction** — peer → relay → peer — plus
//! whatever the relay's own allocation handling adds, and ICMP measures none of that. Routers also
//! deprioritise ICMP, so its jitter is not the link's jitter.
//!
//! This allocates on the server for real, sends game-shaped traffic through the allocation, and
//! times what comes back. The path is exactly the one a relayed player's packets take:
//!
//! ```text
//! probe socket ──▶ TURN ──▶ allocation ──▶ (echo) ──▶ TURN ──▶ probe socket
//! ```
//!
//! Both ends run here, so the measurement is two players who are the same distance from the relay
//! — which is the case that matters when the complaint is "we were in the same room and could not
//! connect".
//!
//! # What it reports, and why each one is here
//!
//! | Metric | Why it matters to this game |
//! |---|---|
//! | STUN round trip | The control. One crossing, no relaying, so relayed − 2×STUN is the relay's own overhead. |
//! | Allocation time | How long a join stalls before the first packet can flow. Sits under the 15 s join timeout. |
//! | Relayed RTT (p50/p95/p99) | What the round trip becomes for a relayed peer. Compare against whatever your netcode is tuned for. |
//! | Jitter | A prediction buffer is sized from jitter, so this costs more than its milliseconds. |
//! | Loss | A dropped input is a swallowed action; whatever a client does with missing input, it does here. |
//! | Out of order | A packet arriving after the tick it was stamped for is a packet nothing reads. |
//!
//! # Limits, stated up front
//!
//! Only `turn:` over **UDP** is measured. `turns:` on 443 — the thing that gets through networks
//! blocking UDP outright — needs a TLS client this does not have, so a server can pass here and
//! still be the right or wrong answer for those players. A `turns:` target is refused rather than
//! silently measured over the wrong transport.
//!
//! # Use
//!
//! ```sh
//! # The free public relay, by its preset
//! cargo run --example relay_probe -- --preset openrelay
//!
//! # Anything else, and several at once for a comparison table
//! cargo run --example relay_probe -- \
//!     --target openrelay --url turn:openrelay.metered.ca:3478 \
//!         --user openrelayproject --pass openrelayproject \
//!     --target ours --url turn:signal.sigma-dev.eu:3478 --user 1757400000 --pass 'HMAC=='
//!
//! # A longer, heavier run, as JSON for a script to keep
//! cargo run --example relay_probe -- --preset openrelay --duration 60 --size 400 --json
//! ```
//!
//! Defaults describe a typical tick-based game: 64 Hz and a 300-byte payload. Set `--rate` and
//! `--size` to what yours actually sends, and `--budget` / `--cliff` to the round trips your
//! netcode is designed for and breaks at, so the verdict line means something for your project.

use std::collections::HashSet;
use std::error::Error;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use turn::auth::{AuthHandler, generate_auth_key, generate_long_term_credentials};
use turn::client::{Client, ClientConfig};
use turn::relay::relay_static::RelayAddressGeneratorStatic;
use turn::server::Server;
use turn::server::config::{ConnConfig, ServerConfig};
use webrtc_util::Conn;
use webrtc_util::vnet::net::Net;

/// A tick rate typical of a rollback game, so the load has a sensible shape out of the box.
/// `--rate` overrides it.
const DEFAULT_RATE_HZ: f64 = 64.0;

/// The round trip a consumer's netcode is assumed to be tuned for, until `--budget` says
/// otherwise. Only the verdict line reads it; every measured number is reported regardless.
const DEFAULT_BUDGET_MS: f64 = 50.0;

/// The round trip past which a consumer's netcode is assumed to struggle, until `--cliff` says
/// otherwise. A six-tick prediction buffer at 64 Hz runs out around here, which is where the
/// default comes from — but it is a property of the consumer, not of this tool.
const DEFAULT_CLIFF_MS: f64 = 190.0;

/// How many STUN binding requests the control takes.
const STUN_PROBES: usize = 8;

/// Long enough for the last packets in flight to land before the receiver is torn down.
const DRAIN: Duration = Duration::from_millis(750);

/// A public STUN server used once, at the start, to prove this machine can speak UDP at all.
///
/// Without it a relay that serves no UDP and a network that blocks UDP produce the same silence,
/// and the difference between "this relay is wrong for us" and "my office Wi-Fi is wrong for this
/// measurement" is the entire conclusion. Overridable with `--control`.
const DEFAULT_CONTROL: &str = "stun.cloudflare.com:3478";

/// What the `loopback` preset's in-process server accepts.
const LOOPBACK_USER: &str = "probe";
const LOOPBACK_PASS: &str = "probe";
const LOOPBACK_REALM: &str = "relay-probe";

// --- What to measure ------------------------------------------------------------------------------

#[derive(Clone)]
struct Target {
    name: String,
    url: String,
    username: String,
    password: String,
    /// Start an in-process TURN server and probe that instead of reaching the network.
    loopback: bool,
    /// Mint the credentials from this shared secret rather than taking them literally.
    ///
    /// A relay run under `use-auth-secret` — which is what `examples/signalling_server.rs` does —
    /// stores no user list: the username is an expiry and the password is an HMAC of it. Without
    /// this, probing your own relay would mean computing that pair by hand.
    secret: Option<String>,
}

struct Options {
    targets: Vec<Target>,
    duration: Duration,
    rate: f64,
    size: usize,
    json: bool,
    control: String,
    budget_ms: f64,
    cliff_ms: f64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            duration: Duration::from_secs(20),
            rate: DEFAULT_RATE_HZ,
            size: 300,
            json: false,
            control: DEFAULT_CONTROL.into(),
            budget_ms: DEFAULT_BUDGET_MS,
            cliff_ms: DEFAULT_CLIFF_MS,
        }
    }
}

/// The credentials Metered publishes for the free Open Relay project.
///
/// Hard-coded because they are public — that is the whole nature of that service — and having the
/// preset means the common comparison is one flag rather than a line of pasted secrets. If they
/// stop working, that is itself the finding, and the probe will say so.
fn preset(name: &str) -> Option<Target> {
    match name {
        "openrelay" => Some(Target {
            name: "openrelay".into(),
            url: "turn:openrelay.metered.ca:3478".into(),
            username: "openrelayproject".into(),
            password: "openrelayproject".into(),
            loopback: false,
            secret: None,
        }),
        // The floor. An in-process relay on `127.0.0.1` has no network under it, so what it
        // reports is the cost of allocating, relaying and echoing and nothing else — which is
        // both the baseline every real relay should be read against, and the check that a
        // surprising result is the relay's rather than this program's.
        //
        // It is also a preview: the same `turn` crate runs the server, so this is roughly what
        // hosting a relay inside the signalling server would behave like, minus the distance.
        "loopback" => Some(Target {
            name: "loopback".into(),
            url: String::new(),
            username: LOOPBACK_USER.into(),
            password: LOOPBACK_PASS.into(),
            loopback: true,
            secret: None,
        }),
        _ => None,
    }
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--preset" => {
                let name = value()?;
                let target = preset(&name)
                    .ok_or(format!("unknown preset {name}; known: openrelay, loopback"))?;
                options.targets.push(target);
            }
            "--target" => options.targets.push(Target {
                name: value()?,
                url: String::new(),
                username: String::new(),
                password: String::new(),
                loopback: false,
                secret: None,
            }),
            "--url" | "--user" | "--pass" | "--secret" => {
                let v = value()?;
                let target = options
                    .targets
                    .last_mut()
                    .ok_or(format!("{arg} must follow a --target"))?;
                match arg.as_str() {
                    "--url" => target.url = v,
                    "--user" => target.username = v,
                    "--secret" => target.secret = Some(v),
                    _ => target.password = v,
                }
            }
            "--duration" => {
                options.duration = Duration::from_secs_f64(
                    value()?.parse().map_err(|_| "--duration wants seconds")?,
                )
            }
            "--rate" => options.rate = value()?.parse().map_err(|_| "--rate wants Hz")?,
            "--size" => options.size = value()?.parse().map_err(|_| "--size wants bytes")?,
            "--control" => options.control = value()?,
            "--budget" => {
                options.budget_ms = value()?
                    .parse()
                    .map_err(|_| "--budget wants milliseconds")?
            }
            "--cliff" => {
                options.cliff_ms = value()?.parse().map_err(|_| "--cliff wants milliseconds")?
            }
            "--json" => options.json = true,
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown argument {other}\n\n{}", usage())),
        }
    }
    if options.targets.is_empty() {
        return Err(format!("nothing to probe\n\n{}", usage()));
    }
    for target in &options.targets {
        if target.url.is_empty() && !target.loopback {
            return Err(format!("target {} has no --url", target.name));
        }
    }
    for target in &options.targets {
        if target.secret.is_some() && !target.username.is_empty() {
            return Err(format!(
                "target {} has both --secret and --user; the secret mints the username, so pass \
                 one or the other",
                target.name
            ));
        }
    }
    if options.size < 8 {
        return Err("--size must be at least 8: the payload carries a sequence number".into());
    }
    Ok(options)
}

fn usage() -> String {
    "usage: relay_probe [--preset openrelay|loopback] \
     [--target NAME --url turn:host:port (--user U --pass P | --secret S)]... \
     [--duration SECS] [--rate HZ] [--size BYTES] [--budget MS] [--cliff MS] \
     [--control HOST:PORT] [--json]"
        .into()
}

// --- Statistics -----------------------------------------------------------------------------------

/// A distribution, in milliseconds.
///
/// Percentiles rather than a mean and a standard deviation alone: what a player feels is the bad
/// packets, and a mean hides them. p99 is the one that decides whether a relay is merely slower or
/// occasionally broken.
#[derive(Default, Clone)]
struct Stats {
    n: usize,
    min: f64,
    p50: f64,
    p90: f64,
    p95: f64,
    p99: f64,
    max: f64,
    mean: f64,
    stddev: f64,
    /// Mean absolute difference between consecutive round trips. What a jitter buffer has to
    /// absorb, in the plainest form.
    jitter_mean: f64,
    /// The same quantity as RFC 3550 smooths it (`J += (|D| - J) / 16`), which is what most
    /// network tooling calls "jitter" — here so numbers can be compared with other tools.
    jitter_rfc3550: f64,
}

impl Stats {
    /// `samples` must be ordered by sequence number, not by value: the jitter figures read
    /// consecutive pairs and mean nothing on a sorted list.
    fn of(samples: &[f64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let n = samples.len();
        let mean = samples.iter().sum::<f64>() / n as f64;
        let variance = samples.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / n as f64;

        let mut deltas = 0.0;
        let mut smoothed = 0.0;
        for pair in samples.windows(2) {
            let d = (pair[1] - pair[0]).abs();
            deltas += d;
            smoothed += (d - smoothed) / 16.0;
        }

        let mut sorted = samples.to_vec();
        sorted.sort_by(f64::total_cmp);
        let at = |q: f64| {
            let i = ((n as f64 - 1.0) * q).round() as usize;
            sorted[i]
        };

        Self {
            n,
            min: sorted[0],
            p50: at(0.50),
            p90: at(0.90),
            p95: at(0.95),
            p99: at(0.99),
            max: sorted[n - 1],
            mean,
            stddev: variance.sqrt(),
            jitter_mean: if n > 1 { deltas / (n - 1) as f64 } else { 0.0 },
            jitter_rfc3550: smoothed,
        }
    }
}

struct Report {
    name: String,
    url: String,
    server: String,
    stun: Stats,
    allocate_ms: f64,
    relayed: Stats,
    sent: usize,
    received: usize,
    duplicates: usize,
    out_of_order: usize,
    error: Option<String>,
}

impl Report {
    fn failed(target: &Target, error: impl std::fmt::Display) -> Self {
        Self {
            name: target.name.clone(),
            url: target.url.clone(),
            server: String::new(),
            stun: Stats::default(),
            allocate_ms: 0.0,
            relayed: Stats::default(),
            sent: 0,
            received: 0,
            duplicates: 0,
            out_of_order: 0,
            error: Some(error.to_string()),
        }
    }

    fn loss_pct(&self) -> f64 {
        if self.sent == 0 {
            return 0.0;
        }
        let delivered = self.received.min(self.sent);
        (self.sent - delivered) as f64 * 100.0 / self.sent as f64
    }

    /// The relay's own cost: everything the round trip spends that is not two crossings of the
    /// network path the STUN control already measured.
    fn overhead_ms(&self) -> f64 {
        (self.relayed.p50 - 2.0 * self.stun.p50).max(0.0)
    }
}

// --- Is UDP even leaving this machine? ---------------------------------------------------------------

/// One STUN binding request, by hand, returning the round trip.
///
/// Hand-rolled rather than built on `turn::client::Client` because the control has to be able to
/// answer "can this host speak UDP at all" without a client, a listen task or a credential in the
/// way. A binding request is twenty bytes and its reply is recognised by the transaction id.
async fn raw_binding(server: SocketAddr, timeout: Duration) -> Option<f64> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.ok()?;

    // A transaction id only has to be unlikely to collide with another in flight on this socket,
    // and this socket carries exactly one. The clock is enough; a random dependency is not.
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    let mut request = Vec::with_capacity(20);
    request.extend_from_slice(&0x0001u16.to_be_bytes()); // Binding request
    request.extend_from_slice(&0u16.to_be_bytes()); // no attributes
    request.extend_from_slice(&0x2112_A442u32.to_be_bytes()); // magic cookie
    request.extend_from_slice(&nanos.to_be_bytes());
    request.extend_from_slice(&0xA5A5_A5A5u32.to_be_bytes());
    let transaction = request[8..20].to_vec();

    let started = Instant::now();
    socket.send_to(&request, server).await.ok()?;
    let mut buf = vec![0u8; 512];
    loop {
        let (n, _) = tokio::time::timeout(timeout, socket.recv_from(&mut buf))
            .await
            .ok()?
            .ok()?;
        // 0x0101 is a binding success response. Anything else on this socket is not ours.
        if n >= 20 && buf[..2] == [0x01, 0x01] && buf[8..20] == transaction[..] {
            return Some(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
}

/// Whether a TCP connection opens on the same address, which is what separates "this server is
/// down" from "this server does not serve UDP".
async fn tcp_reachable(server: SocketAddr) -> bool {
    tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(server))
        .await
        .map(|result| result.is_ok())
        .unwrap_or(false)
}

/// Why nothing answered on UDP, in the most specific terms the evidence supports.
async fn diagnose_no_udp(server: SocketAddr, control: Option<f64>) -> String {
    let tcp = tcp_reachable(server).await;
    match (tcp, control) {
        (true, Some(_)) => format!(
            "no UDP answer from {server}, but TCP opens there. The relay serves TCP and not UDP, \
             and this machine's UDP is fine (the control replied). TURN over TCP means a lost \
             packet stalls every packet behind it, which is the opposite of what a realtime game \
             wants — treat this relay as unsuitable rather than untested."
        ),
        (true, None) => format!(
            "no UDP answer from {server}; TCP opens there, but the UDP control failed too, so \
             this machine's own UDP path is suspect. Re-run somewhere else before blaming the relay."
        ),
        (false, Some(_)) => format!(
            "nothing answered at {server} on UDP or TCP. The control replied, so UDP works here — \
             the address is wrong, or the server is down."
        ),
        (false, None) => format!(
            "nothing answered at {server}, and the UDP control failed as well. Check this \
             machine's network before reading anything into it."
        ),
    }
}

// --- The floor: a relay with no network under it ------------------------------------------------------

/// Accepts one fixed credential, the way a server run for a measurement should.
struct StaticAuth {
    password: String,
}

impl AuthHandler for StaticAuth {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        _src_addr: SocketAddr,
    ) -> Result<Vec<u8>, turn::Error> {
        Ok(generate_auth_key(username, realm, &self.password))
    }
}

/// Start an in-process TURN server on the loopback interface, returning it and its address.
///
/// The server has to be held for as long as it is probed — dropping it takes the relay with it —
/// so it is returned rather than forgotten inside this function.
async fn start_loopback_server() -> Result<(Server, SocketAddr), Box<dyn Error>> {
    let conn = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let address = conn.local_addr()?;
    let server = Server::new(ServerConfig {
        conn_configs: vec![ConnConfig {
            conn,
            relay_addr_generator: Box::new(RelayAddressGeneratorStatic {
                relay_address: IpAddr::from_str("127.0.0.1")?,
                address: "127.0.0.1".to_owned(),
                net: Arc::new(Net::new(None)),
            }),
        }],
        realm: LOOPBACK_REALM.to_owned(),
        auth_handler: Arc::new(StaticAuth {
            password: LOOPBACK_PASS.to_owned(),
        }),
        channel_bind_timeout: Duration::from_secs(0),
        alloc_close_notify: None,
    })
    .await?;
    Ok((server, address))
}

// --- Running one target ---------------------------------------------------------------------------

/// Split `turn:host:port?transport=udp` into a host:port, refusing what cannot be measured here.
fn parse_url(url: &str) -> Result<String, String> {
    let (scheme, rest) = url
        .split_once(':')
        .ok_or(format!("{url} is not a TURN URL"))?;
    let rest = rest.split('?').next().unwrap_or(rest);
    if let Some(query) = url.split_once('?').map(|(_, q)| q)
        && query.contains("transport=tcp")
    {
        return Err(
            "this probe measures UDP only; drop ?transport=tcp to compare like for like".into(),
        );
    }
    match scheme {
        "turn" => {}
        "turns" => {
            return Err(
                "turns: is TLS, which this probe has no client for. It is exactly the transport \
                 that matters for UDP-blocked networks, so measure it elsewhere rather than \
                 assuming this result covers it."
                    .into(),
            );
        }
        "stun" => return Err("that is a STUN URL; a relay measurement needs turn:".into()),
        other => return Err(format!("unsupported scheme {other}:")),
    }
    if !rest.contains(':') {
        return Err(format!("{rest} has no port; TURN is usually 3478"));
    }
    Ok(rest.to_string())
}

async fn resolve(host_port: &str) -> Result<SocketAddr, String> {
    tokio::net::lookup_host(host_port)
        .await
        .map_err(|e| format!("could not resolve {host_port}: {e}"))?
        // IPv4 first: the crate's client takes a single address, and a v6 answer on a host with no
        // v6 route is the failure mode `bevy_ensemble_webrtc`'s docs already warn about.
        .find(|addr| addr.is_ipv4())
        .ok_or(format!("{host_port} resolved to no IPv4 address"))
}

async fn run(
    target: &Target,
    options: &Options,
    control: Option<f64>,
) -> Result<Report, Box<dyn Error>> {
    let host_port = parse_url(&target.url)?;
    let server = resolve(&host_port).await?;

    // --- The control, and the probe socket's public address -------------------------------------
    //
    // Both come from the same binding requests. The address is not incidental: the relay will not
    // forward anything to a peer it has no permission for, and a permission is created for a
    // public address, which a socket behind NAT cannot know about itself.
    let probe = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    let discovery = Client::new(ClientConfig {
        stun_serv_addr: server.to_string(),
        turn_serv_addr: server.to_string(),
        username: String::new(),
        password: String::new(),
        realm: String::new(),
        software: String::new(),
        rto_in_ms: 300,
        conn: Arc::clone(&probe) as Arc<dyn Conn + Send + Sync>,
        vnet: None,
    })
    .await?;
    discovery.listen().await?;

    let mut stun_samples = Vec::new();
    let mut public = None;
    for _ in 0..STUN_PROBES {
        let started = Instant::now();
        if let Ok(addr) = discovery.send_binding_request().await {
            stun_samples.push(started.elapsed().as_secs_f64() * 1000.0);
            public = Some(addr);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Stops the read loop and leaves the socket open — `close` cancels its token and clears the
    // transaction map, and never touches `conn`. That is what lets the probe socket be reused
    // below as a plain UDP socket with nothing competing for its packets.
    discovery.close().await?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let Some(public) = public else {
        return Err(diagnose_no_udp(server, control).await.into());
    };

    // --- The allocation --------------------------------------------------------------------------
    //
    // A secret mints a pair the way the relay will check it: username is an expiry, password the
    // HMAC of that expiry. An hour is far longer than a probe run and short enough that a pair
    // pasted into a shell history is worth nothing tomorrow.
    let (username, password) = match &target.secret {
        Some(secret) => generate_long_term_credentials(secret, Duration::from_secs(3600))
            .map_err(|error| format!("could not mint credentials from --secret: {error}"))?,
        None => (target.username.clone(), target.password.clone()),
    };
    let relay_socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    let client = Client::new(ClientConfig {
        stun_serv_addr: server.to_string(),
        turn_serv_addr: server.to_string(),
        username,
        password,
        realm: String::new(),
        software: String::new(),
        rto_in_ms: 300,
        conn: Arc::clone(&relay_socket) as Arc<dyn Conn + Send + Sync>,
        vnet: None,
    })
    .await?;
    client.listen().await?;

    let started = Instant::now();
    // Wrapped, because the bare error from the crate is a decoding detail — "attribute not
    // found" is what a STUN-only server produces — and the useful question is which of the two
    // ordinary causes it was.
    let relay = client.allocate().await.map_err(|error| {
        format!(
            "{server} refused the allocation: {error}. Either the credentials are wrong, or this \
             address answers STUN but does not serve TURN."
        )
    })?;
    let relay = Arc::new(relay);
    let allocate_ms = started.elapsed().as_secs_f64() * 1000.0;
    let relayed_addr = relay.local_addr()?;

    // Permission for the probe, or every packet it sends is discarded by the relay in silence.
    relay.send_to(&[0u8], public).await?;

    // --- Echo, and the run ------------------------------------------------------------------------
    let echo = tokio::spawn({
        let relay = Arc::clone(&relay);
        async move {
            let mut buf = vec![0u8; 2048];
            while let Ok((n, from)) = relay.recv_from(&mut buf).await {
                if relay.send_to(&buf[..n], from).await.is_err() {
                    break;
                }
            }
        }
    });

    let (arrivals_tx, mut arrivals) = mpsc::unbounded_channel();
    let receiver = tokio::spawn({
        let probe = Arc::clone(&probe);
        async move {
            let mut buf = vec![0u8; 2048];
            while let Ok((n, _)) = probe.recv_from(&mut buf).await {
                if n >= 4 {
                    let seq = u32::from_le_bytes(buf[..4].try_into().expect("4 bytes"));
                    if arrivals_tx.send((seq, Instant::now())).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let total = (options.duration.as_secs_f64() * options.rate).max(1.0) as u32;
    let mut sent_at: Vec<Option<Instant>> = vec![None; total as usize];
    let mut payload = vec![0u8; options.size];
    let mut ticker = tokio::time::interval(Duration::from_secs_f64(1.0 / options.rate));
    for seq in 0..total {
        ticker.tick().await;
        payload[..4].copy_from_slice(&seq.to_le_bytes());
        sent_at[seq as usize] = Some(Instant::now());
        let _ = probe.send_to(&payload, relayed_addr).await;
    }

    tokio::time::sleep(DRAIN).await;
    receiver.abort();
    echo.abort();
    let _ = client.close().await;

    // --- What came back ----------------------------------------------------------------------------
    arrivals.close();
    let mut seen = HashSet::new();
    let mut duplicates = 0;
    let mut out_of_order = 0;
    let mut highest = 0u32;
    let mut by_seq: Vec<(u32, f64)> = Vec::new();
    while let Some((seq, at)) = arrivals.recv().await {
        let Some(Some(sent)) = sent_at.get(seq as usize) else {
            continue;
        };
        if !seen.insert(seq) {
            duplicates += 1;
            continue;
        }
        if seq < highest {
            out_of_order += 1;
        }
        highest = highest.max(seq);
        by_seq.push((seq, at.duration_since(*sent).as_secs_f64() * 1000.0));
    }
    by_seq.sort_by_key(|(seq, _)| *seq);
    let round_trips: Vec<f64> = by_seq.iter().map(|(_, rtt)| *rtt).collect();

    Ok(Report {
        name: target.name.clone(),
        url: target.url.clone(),
        server: server.to_string(),
        stun: Stats::of(&stun_samples),
        allocate_ms,
        relayed: Stats::of(&round_trips),
        sent: total as usize,
        received: round_trips.len(),
        duplicates,
        out_of_order,
        error: None,
    })
}

// --- Saying what happened ---------------------------------------------------------------------------

fn print_report(report: &Report, options: &Options) {
    println!(
        "\n── {} ──────────────────────────────────────────",
        report.name
    );
    println!("  url               {}", report.url);
    if let Some(error) = &report.error {
        println!("  FAILED            {error}");
        return;
    }
    println!("  server            {}", report.server);
    println!(
        "  stun rtt          {:.1} ms  (min {:.1}, p95 {:.1}) — one crossing, the control",
        report.stun.p50, report.stun.min, report.stun.p95
    );
    println!(
        "  allocation        {:.0} ms  — a join waits this long before any packet can flow",
        report.allocate_ms
    );
    println!(
        "  load              {:.0} Hz × {} B for {:.0}s = {} packets",
        options.rate,
        options.size,
        options.duration.as_secs_f64(),
        report.sent
    );

    let r = &report.relayed;
    if r.n == 0 {
        println!(
            "  relayed rtt       nothing came back — allocation succeeded but no traffic returned"
        );
        return;
    }
    println!(
        "  relayed rtt       min {:.1}  p50 {:.1}  p90 {:.1}  p95 {:.1}  p99 {:.1}  max {:.1} ms",
        r.min, r.p50, r.p90, r.p95, r.p99, r.max
    );
    println!(
        "                    mean {:.1}  sd {:.1}   over {} samples",
        r.mean, r.stddev, r.n
    );
    println!(
        "  jitter            {:.1} ms mean step   ({:.1} ms RFC 3550)",
        r.jitter_mean, r.jitter_rfc3550
    );
    println!(
        "  loss              {:.2}%   duplicates {}   out of order {}",
        report.loss_pct(),
        report.duplicates,
        report.out_of_order
    );
    println!(
        "  relay overhead    {:.1} ms  (p50 relayed − 2 × p50 stun)",
        report.overhead_ms()
    );
    // What a relayed player costs the machine hosting the relay. Doubled because the relay both
    // receives and re-sends every packet, and again because a session has traffic in both
    // directions — the figure a VPS's bandwidth allowance has to be read against.
    let one_way = options.rate * options.size as f64;
    println!(
        "  relay bandwidth   {:.0} kB/s per relayed player ({:.1} GB per 100 player-hours)",
        one_way * 4.0 / 1000.0,
        one_way * 4.0 * 3600.0 * 100.0 / 1e9
    );

    // --- The same numbers in the consumer's own units ---
    let tick_ms = 1000.0 / options.rate;
    println!(
        "  in ticks @{:.0}Hz   p50 {:.1}  p95 {:.1}  ({:.2} ms per tick)",
        options.rate,
        r.p50 / tick_ms,
        r.p95 / tick_ms,
        tick_ms
    );
    let (budget, cliff) = (options.budget_ms, options.cliff_ms);
    let verdict = if r.p95 >= cliff {
        format!(
            "p95 is past the {cliff:.0} ms this project was told it breaks at — expect the prediction buffer to struggle"
        )
    } else if r.p95 >= budget * 2.0 {
        format!("p95 is over twice the {budget:.0} ms budget; playable, with more correction")
    } else {
        format!("p95 sits under twice the {budget:.0} ms budget — comfortably inside it")
    };
    println!("  verdict           {verdict}");
}

fn print_comparison(reports: &[Report]) {
    let usable: Vec<&Report> = reports
        .iter()
        .filter(|r| r.error.is_none() && r.relayed.n > 0)
        .collect();
    if usable.len() < 2 {
        return;
    }
    println!("\n── comparison ──────────────────────────────────────────");
    println!(
        "  {:<16} {:>9} {:>9} {:>9} {:>9} {:>8} {:>9}",
        "target", "p50 ms", "p95 ms", "p99 ms", "jitter", "loss", "alloc ms"
    );
    for r in &usable {
        println!(
            "  {:<16} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>7.2}% {:>9.0}",
            r.name,
            r.relayed.p50,
            r.relayed.p95,
            r.relayed.p99,
            r.relayed.jitter_mean,
            r.loss_pct(),
            r.allocate_ms
        );
    }
    let best = usable
        .iter()
        .min_by(|a, b| a.relayed.p95.total_cmp(&b.relayed.p95))
        .expect("usable is non-empty");
    println!("\n  lowest p95: {}", best.name);
}

/// Hand-rolled rather than a serde derive: this is a dev tool, the shape is nine numbers, and a
/// dependency that exists only to print them would be carried by every `cargo test` in the
/// workspace.
fn print_json(reports: &[Report], options: &Options) {
    let stats = |s: &Stats| {
        format!(
            r#"{{"n":{},"min":{:.3},"p50":{:.3},"p90":{:.3},"p95":{:.3},"p99":{:.3},"max":{:.3},"mean":{:.3},"stddev":{:.3},"jitter_mean":{:.3},"jitter_rfc3550":{:.3}}}"#,
            s.n,
            s.min,
            s.p50,
            s.p90,
            s.p95,
            s.p99,
            s.max,
            s.mean,
            s.stddev,
            s.jitter_mean,
            s.jitter_rfc3550
        )
    };
    println!("{{");
    println!(
        r#"  "load": {{"rate_hz": {:.1}, "payload_bytes": {}, "duration_s": {:.1}}},"#,
        options.rate,
        options.size,
        options.duration.as_secs_f64()
    );
    println!(r#"  "targets": ["#);
    for (i, r) in reports.iter().enumerate() {
        let comma = if i + 1 == reports.len() { "" } else { "," };
        match &r.error {
            Some(error) => println!(
                r#"    {{"name":"{}","url":"{}","error":"{}"}}{comma}"#,
                r.name,
                r.url,
                error.replace('"', "'").replace('\n', " ")
            ),
            None => println!(
                r#"    {{"name":"{}","url":"{}","server":"{}","allocate_ms":{:.1},"sent":{},"received":{},"loss_pct":{:.3},"duplicates":{},"out_of_order":{},"overhead_ms":{:.3},"stun":{},"relayed":{}}}{comma}"#,
                r.name,
                r.url,
                r.server,
                r.allocate_ms,
                r.sent,
                r.received,
                r.loss_pct(),
                r.duplicates,
                r.out_of_order,
                r.overhead_ms(),
                stats(&r.stun),
                stats(&r.relayed)
            ),
        }
    }
    println!("  ]");
    println!("}}");
}

#[tokio::main]
async fn main() {
    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    // The control first: everything below is read differently depending on whether UDP leaves
    // this machine at all.
    let control = match resolve(&options.control).await {
        Ok(address) => raw_binding(address, Duration::from_secs(3)).await,
        Err(_) => None,
    };
    if !options.json {
        match control {
            Some(rtt) => eprintln!(
                "udp control  {} answered in {rtt:.1} ms — this machine can speak UDP",
                options.control
            ),
            None => eprintln!(
                "udp control  {} did not answer. UDP may be blocked here; any silent relay \
                 below is unproven rather than bad.",
                options.control
            ),
        }
    }

    let mut reports = Vec::new();
    // Held only to keep any loopback server alive for the length of the run.
    let mut servers = Vec::new();
    for target in &options.targets {
        let mut target = target.clone();
        if target.loopback {
            match start_loopback_server().await {
                Ok((server, address)) => {
                    target.url = format!("turn:{address}");
                    servers.push(server);
                }
                Err(error) => {
                    reports.push(Report::failed(&target, error));
                    continue;
                }
            }
        }
        if !options.json {
            eprintln!(
                "probing {} ({}) for {:.0}s...",
                target.name,
                target.url,
                options.duration.as_secs_f64()
            );
        }
        let report = match run(&target, &options, control).await {
            Ok(report) => report,
            Err(error) => Report::failed(&target, error),
        };
        reports.push(report);
    }
    for server in servers {
        let _ = server.close().await;
    }

    if options.json {
        print_json(&reports, &options);
    } else {
        for report in &reports {
            print_report(report, &options);
        }
        print_comparison(&reports);
        println!();
    }

    if reports.iter().all(|r| r.error.is_some()) {
        std::process::exit(1);
    }
}
