//! Lightweight, backend-agnostic network counters.
//!
//! Enabled by the `netmetrics` feature. Counting happens at the two seams every
//! platform backend already funnels through:
//!
//! - **inbound**: [`decode_ensemble_packet`](crate::decode_ensemble_packet), called
//!   by every backend when bytes arrive.
//! - **outbound**: the [`SerializedLobbyPacket`](crate::SerializedLobbyPacket) event,
//!   triggered by the core encoding pipeline before a backend transmits.
//!
//! Because both seams live in `bevy_ensemble` itself, the WebRTC and Steam backends
//! are both measured with zero backend-specific code.
//!
//! The cost when enabled is a few integer additions per packet; when the feature is
//! off, none of this compiles in.

use bevy::prelude::*;

use crate::SerializedLobbyPacket;

/// Installs the [`NetMetrics`] resource, the outbound-packet observer and the rate
/// sampler. Added by `EnsemblePlugin` when the `netmetrics` feature is on (which the
/// `netdebug` feature implies).
pub struct NetMetricsPlugin;

impl Plugin for NetMetricsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<NetMetrics>()
            .add_observer(count_outbound)
            .add_systems(Update, sample_rates);
    }
}

/// Rolling network counters for the local peer, exposed as a resource.
///
/// Cumulative totals are updated inline at the packet boundary. The `*_per_sec`
/// fields are sampled once per [`SAMPLE_INTERVAL`] by [`sample_rates`] so the debug
/// overlay has smooth values to display without doing per-frame math.
#[derive(Resource, Debug, Default)]
pub struct NetMetrics {
    /// Total bytes handed to `decode_ensemble_packet` since startup.
    pub rx_bytes: u64,
    /// Total inbound packets since startup.
    pub rx_packets: u64,
    /// Total bytes emitted as `SerializedLobbyPacket` since startup.
    pub tx_bytes: u64,
    /// Total outbound packets since startup.
    pub tx_packets: u64,
    /// Inbound packets dropped by the network simulator (`netdebug` only).
    pub sim_dropped: u64,
    /// Inbound packets duplicated by the network simulator (`netdebug` only).
    pub sim_duplicated: u64,

    /// Sampled inbound throughput (bytes/sec).
    pub rx_bytes_per_sec: f64,
    /// Sampled outbound throughput (bytes/sec).
    pub tx_bytes_per_sec: f64,
    /// Sampled inbound packet rate (packets/sec).
    pub rx_packets_per_sec: f64,
    /// Sampled outbound packet rate (packets/sec).
    pub tx_packets_per_sec: f64,

    // Snapshot of cumulative counters at the last sample, for delta computation.
    last_rx_bytes: u64,
    last_tx_bytes: u64,
    last_rx_packets: u64,
    last_tx_packets: u64,
    accum_secs: f64,
}

/// How often [`sample_rates`] recomputes the per-second fields.
const SAMPLE_INTERVAL: f64 = 0.5;

/// Global observer: count every serialized outbound packet.
///
/// Registered on the app so it fires for packets produced by any backend.
pub(crate) fn count_outbound(packet: On<SerializedLobbyPacket>, metrics: Option<ResMut<NetMetrics>>) {
    if let Some(mut metrics) = metrics {
        metrics.tx_bytes += packet.packet.len() as u64;
        metrics.tx_packets += 1;
    }
}

/// Recompute the sampled per-second rates from cumulative deltas.
pub(crate) fn sample_rates(mut metrics: ResMut<NetMetrics>, time: Res<Time>) {
    metrics.accum_secs += time.delta_secs_f64();
    if metrics.accum_secs < SAMPLE_INTERVAL {
        return;
    }
    let dt = metrics.accum_secs;
    metrics.accum_secs = 0.0;

    let d_rx_bytes = metrics.rx_bytes - metrics.last_rx_bytes;
    let d_tx_bytes = metrics.tx_bytes - metrics.last_tx_bytes;
    let d_rx_packets = metrics.rx_packets - metrics.last_rx_packets;
    let d_tx_packets = metrics.tx_packets - metrics.last_tx_packets;

    metrics.rx_bytes_per_sec = d_rx_bytes as f64 / dt;
    metrics.tx_bytes_per_sec = d_tx_bytes as f64 / dt;
    metrics.rx_packets_per_sec = d_rx_packets as f64 / dt;
    metrics.tx_packets_per_sec = d_tx_packets as f64 / dt;

    metrics.last_rx_bytes = metrics.rx_bytes;
    metrics.last_tx_bytes = metrics.tx_bytes;
    metrics.last_rx_packets = metrics.rx_packets;
    metrics.last_tx_packets = metrics.tx_packets;
}
