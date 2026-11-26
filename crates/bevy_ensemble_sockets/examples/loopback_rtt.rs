//! Standalone WebRTC data-channel loopback RTT benchmark.
//!
//! Creates two `webrtc` peer connections **in one process**, wires their ICE/SDP directly
//! to each other (no signalling server), and ping-pongs a small payload over the exact same
//! unreliable data channel config the crate uses in production (`ordered:false`,
//! `max_retransmits:0`, negotiated id 1). Everything is timed with a single [`Instant`], so
//! there is no clock skew, no Bevy frame loop, no vsync, and no overlay smoothing.
//!
//! This isolates the raw transport: whatever RTT this prints is webrtc-rs's SCTP/DTLS/UDP
//! cost on loopback and nothing else. Run with:
//!
//! ```sh
//! cargo run --release --example loopback_rtt -p bevy_ensemble_sockets
//! ```

#[cfg(not(target_arch = "wasm32"))]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::{Arc, OnceLock};
    use std::time::{Duration, Instant};

    use bytes::BytesMut;
    use tokio::sync::{mpsc, oneshot};
    use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelInit};
    use webrtc::peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
        RTCPeerConnectionIceEvent,
    };

    const WARMUP: usize = 50;
    const SAMPLES: usize = 500;
    const PAYLOAD_BYTES: usize = 64;

    /// Trickles each local candidate straight into the other peer (same process, no
    /// serialization beyond the crate's own `to_json`).
    struct Trickle(Arc<OnceLock<Arc<dyn PeerConnection>>>);

    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for Trickle {
        async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
            if let (Some(other), Ok(init)) = (self.0.get(), event.candidate.to_json()) {
                let _ = other.add_ice_candidate(init).await;
            }
        }
    }

    // No ICE servers: gather host candidates only, so the pair stays on loopback/LAN and we
    // measure transport, not STUN. (STUN would only affect connection setup anyway.)
    let to_ponger = Arc::new(OnceLock::new());
    let to_pinger = Arc::new(OnceLock::new());
    let build = |trickle: Arc<OnceLock<Arc<dyn PeerConnection>>>| async move {
        PeerConnectionBuilder::new()
            .with_handler(Arc::new(Trickle(trickle)))
            .with_udp_addrs(vec!["0.0.0.0:0"])
            .build()
            .await
            .map(|pc| Arc::new(pc) as Arc<dyn PeerConnection>)
    };
    let pinger = build(to_ponger.clone()).await?;
    let ponger = build(to_pinger.clone()).await?;
    let _ = to_ponger.set(ponger.clone());
    let _ = to_pinger.set(pinger.clone());

    // Same negotiated unreliable channel both sides create (matches native.rs).
    let dc_init = || RTCDataChannelInit {
        ordered: false,
        max_retransmits: Some(0),
        negotiated: Some(1),
        ..Default::default()
    };
    let ping_dc = pinger
        .create_data_channel("ensemble_unreliable", Some(dc_init()))
        .await?;
    let pong_dc = ponger
        .create_data_channel("ensemble_unreliable", Some(dc_init()))
        .await?;

    // Each channel's events: the ponger echoes every message straight back, the pinger forwards
    // each pong to the measurement loop, and both say when they are open.
    let (pong_tx, mut pong_rx) = mpsc::unbounded_channel::<()>();
    let events =
        |dc: Arc<dyn DataChannel>, echo: bool, pongs: Option<mpsc::UnboundedSender<()>>| {
            let (open_tx, open_rx) = oneshot::channel::<()>();
            tokio::spawn(async move {
                let mut open_tx = Some(open_tx);
                while let Some(event) = dc.poll().await {
                    match event {
                        DataChannelEvent::OnOpen => {
                            if let Some(open) = open_tx.take() {
                                let _ = open.send(());
                            }
                        }
                        DataChannelEvent::OnMessage(message) if echo => {
                            let _ = dc.send(message.data).await;
                        }
                        DataChannelEvent::OnMessage(_) => {
                            if let Some(pongs) = &pongs {
                                let _ = pongs.send(());
                            }
                        }
                        _ => {}
                    }
                }
            });
            open_rx
        };
    let ping_open = events(ping_dc.clone(), false, Some(pong_tx));
    let pong_open = events(pong_dc.clone(), true, None);

    // Offer / answer exchange (same process — pass descriptions directly).
    let offer = pinger.create_offer(None).await?;
    pinger.set_local_description(offer.clone()).await?;
    ponger.set_remote_description(offer).await?;
    let answer = ponger.create_answer(None).await?;
    ponger.set_local_description(answer.clone()).await?;
    pinger.set_remote_description(answer).await?;

    println!("connecting...");
    tokio::time::timeout(Duration::from_secs(15), async {
        let _ = ping_open.await;
        let _ = pong_open.await;
    })
    .await
    .map_err(|_| "data channels did not open within 15s")?;
    println!(
        "channels open; measuring {SAMPLES} sequential round trips ({PAYLOAD_BYTES}B, unreliable)..."
    );

    let payload = BytesMut::from(&[0u8; PAYLOAD_BYTES][..]);
    let mut samples: Vec<Duration> = Vec::with_capacity(SAMPLES);
    let mut drops = 0usize;

    for i in 0..(WARMUP + SAMPLES) {
        let start = Instant::now();
        ping_dc.send(payload.clone()).await?;
        // Sequential: send one, wait for its echo before the next. Loopback drops are ~nil;
        // a timeout just skips that sample (unreliable channel, no retransmit).
        match tokio::time::timeout(Duration::from_secs(1), pong_rx.recv()).await {
            Ok(Some(())) => {
                if i >= WARMUP {
                    samples.push(start.elapsed());
                }
            }
            _ => drops += 1,
        }
    }

    if samples.is_empty() {
        return Err("no round trips completed".into());
    }

    samples.sort_unstable();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let pct = |p: f64| {
        let idx = ((samples.len() as f64 * p) as usize).min(samples.len() - 1);
        ms(samples[idx])
    };
    let mean = ms(samples.iter().sum::<Duration>()) / samples.len() as f64;

    println!("\nWebRTC unreliable data-channel RTT (loopback, one process):");
    println!("  samples: {}   drops/timeouts: {drops}", samples.len());
    println!(
        "  min {:.2}ms   p50 {:.2}ms   mean {:.2}ms   p95 {:.2}ms   p99 {:.2}ms   max {:.2}ms",
        ms(samples[0]),
        pct(0.50),
        mean,
        pct(0.95),
        pct(0.99),
        ms(samples[samples.len() - 1]),
    );
    println!(
        "\nThis is pure transport. Compare with the app's overlay rtt/wire to see how much\n\
         the bevy_ensemble layer adds on top.",
    );

    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn main() {}
