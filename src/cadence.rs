//! Doing something every so often, at the rate asked for.
//!
//! The way this crate used to do it — a `Local<f32>` counted down by the frame's delta and set
//! back to the interval when it ran out — loses whatever the frame overshot by, every time. A
//! 100 ms interval at 30 fps ran out 33 ms late and started over from there: ten pings a second
//! became seven and a half, and at 60 fps one went out every six or seven frames depending on how
//! the `f32` rounded. [`Cadence`] keeps the phase: what a frame overshoots comes off the next wait.
//!
//! It still fires at most once a frame. A frame that spans several intervals — a hitch, a tab
//! that slept — fires once and carries on from where the phase would have been, rather than
//! sending the backlog in a burst nobody wants.

use std::time::Duration;

/// A countdown that keeps its phase. See [the module](self).
///
/// Starts due: the first [`tick`](Self::tick) fires, as a new session wants its first ping or
/// report at once rather than an interval in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cadence {
    until_next: Duration,
}

impl Cadence {
    /// Advance by a frame of `delta`. `true` if an `interval` came due during it.
    pub fn tick(&mut self, delta: Duration, interval: Duration) -> bool {
        if delta < self.until_next {
            self.until_next -= delta;
            return false;
        }
        if interval.is_zero() {
            self.until_next = Duration::ZERO;
            return true;
        }
        // Past the moment it was due by this much; the next is that much nearer. Past by more
        // than an interval, the missed ones are skipped, not owed.
        let over = (delta - self.until_next).as_nanos() % interval.as_nanos();
        self.until_next = interval - Duration::from_nanos(over as u64);
        true
    }

    /// Due again at the next tick, as at the start.
    pub fn reset(&mut self) {
        self.until_next = Duration::ZERO;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_millis(100);

    fn fires(cadence: &mut Cadence, frame: Duration, frames: u32) -> u32 {
        (0..frames)
            .filter(|_| cadence.tick(frame, INTERVAL))
            .count() as u32
    }

    #[test]
    fn it_keeps_the_rate_asked_for_whatever_the_frame_rate() {
        // Thirty seconds at 30 fps: 300 intervals. Restarting the countdown on each firing made
        // it 225.
        let mut cadence = Cadence::default();
        let thirtieth = Duration::from_nanos(33_333_333);
        let fired = fires(&mut cadence, thirtieth, 900);
        assert!((299..=301).contains(&fired), "{fired} firings in 30 s");

        let mut cadence = Cadence::default();
        let sixtieth = Duration::from_nanos(16_666_667);
        let fired = fires(&mut cadence, sixtieth, 1800);
        assert!((299..=301).contains(&fired), "{fired} firings in 30 s");
    }

    #[test]
    fn it_fires_at_once_and_a_long_frame_fires_once() {
        let mut cadence = Cadence::default();
        assert!(cadence.tick(Duration::ZERO, INTERVAL), "due at the start");
        assert!(!cadence.tick(Duration::from_millis(50), INTERVAL));
        assert!(
            cadence.tick(Duration::from_secs(3), INTERVAL),
            "three seconds asleep is one firing, not thirty"
        );
        assert!(!cadence.tick(Duration::ZERO, INTERVAL));
        cadence.reset();
        assert!(
            cadence.tick(Duration::ZERO, INTERVAL),
            "and due again after a reset"
        );
    }
}
