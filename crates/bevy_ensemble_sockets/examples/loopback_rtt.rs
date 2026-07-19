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
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use tokio::sync::{Notify, mpsc};
    use webrtc::api::APIBuilder;
    use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
    use webrtc::data_channel::data_channel_message::DataChannelMessage;
    use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
    use webrtc::peer_connection::configuration::RTCConfiguration;

    const WARMUP: usize = 50;
    const SAMPLES: usize = 500;
    const PAYLOAD_BYTES: usize = 64;

    let api = APIBuilder::new().build();
    // No ICE servers: gather host candidates only, so the pair stays on loopback/LAN and we
    // measure transport, not STUN. (STUN would only affect connection setup anyway.)
    let config = || RTCConfiguration::default();

    let pinger = Arc::new(api.new_peer_connection(config()).await?);
    let ponger = Arc::new(api.new_peer_connection(config()).await?);

    // Trickle ICE candidates directly into the other peer (same process, no serialization
    // needed beyond the crate's own to_json round trip).
    {
        let ponger = ponger.clone();
        pinger.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let ponger = ponger.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    if let Ok(init) = c.to_json() {
                        let _ = ponger.add_ice_candidate(init).await;
                    }
                }
            })
        }));
    }
    {
        let pinger = pinger.clone();
        ponger.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let pinger = pinger.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    if let Ok(init) = c.to_json() {
                        let _ = pinger.add_ice_candidate(init).await;
                    }
                }
            })
        }));
    }

    // Same negotiated unreliable channel both sides create (matches native.rs).
    let dc_init = || RTCDataChannelInit {
        ordered: Some(false),
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

    // Ponger echoes every message straight back.
    {
        let pong_dc_echo = pong_dc.clone();
        pong_dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let pong_dc_echo = pong_dc_echo.clone();
            Box::pin(async move {
                let _ = pong_dc_echo.send(&msg.data).await;
            })
        }));
    }

    // Pinger forwards each received pong to the measurement loop.
    let (pong_tx, mut pong_rx) = mpsc::unbounded_channel::<()>();
    ping_dc.on_message(Box::new(move |_msg: DataChannelMessage| {
        let _ = pong_tx.send(());
        Box::pin(async {})
    }));

    // Signal when both channels are open.
    let ping_open = Arc::new(Notify::new());
    let pong_open = Arc::new(Notify::new());
    {
        let n = ping_open.clone();
        ping_dc.on_open(Box::new(move || {
            let n = n.clone();
            Box::pin(async move { n.notify_one() })
        }));
    }
    {
        let n = pong_open.clone();
        pong_dc.on_open(Box::new(move || {
            let n = n.clone();
            Box::pin(async move { n.notify_one() })
        }));
    }

    // Offer / answer exchange (same process — pass descriptions directly).
    let offer = pinger.create_offer(None).await?;
    pinger.set_local_description(offer.clone()).await?;
    ponger.set_remote_description(offer).await?;
    let answer = ponger.create_answer(None).await?;
    ponger.set_local_description(answer.clone()).await?;
    pinger.set_remote_description(answer).await?;

    println!("connecting...");
    tokio::time::timeout(Duration::from_secs(15), async {
        ping_open.notified().await;
        pong_open.notified().await;
    })
    .await
    .map_err(|_| "data channels did not open within 15s")?;
    println!("channels open; measuring {SAMPLES} sequential round trips ({PAYLOAD_BYTES}B, unreliable)...");

    let payload = Bytes::from(vec![0u8; PAYLOAD_BYTES]);
    let mut samples: Vec<Duration> = Vec::with_capacity(SAMPLES);
    let mut drops = 0usize;

    for i in 0..(WARMUP + SAMPLES) {
        let start = Instant::now();
        ping_dc.send(&payload).await?;
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
