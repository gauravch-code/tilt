//! QUIC's congestion control for the stream: quinn's BBR, with a floor under its window.
//!
//! What limits the video in flight is flow control (src/flow.rs): the viewer acknowledges
//! every frame, and no more is encoded than about one and a half bandwidth-delay products of
//! what those ACKs show the path to carry, at a bitrate that follows its queueing delay. QUIC's
//! own controller sits below that, and for a desktop it is the wrong judge:
//!
//! - A desktop is idle, then busy. BBR answers a lost packet by holding the window to what
//!   is in flight for a round ("conservation"), and while a session idles that is next to
//!   nothing: four packets, until something sent after the loss has been acknowledged. The
//!   first frames of a scroll then waited in QUIC's send queue for a whole round trip, a
//!   quarter of a second between continents (in the path statistics: a window of 5808 bytes
//!   and nothing sent for 250 ms while 130 KB queued), on about every other start of motion.
//! - quinn's BBR takes no bandwidth sample while the application sends less than the window
//!   allows, which a desktop stream nearly always does. Its estimate goes stale, and now and
//!   then the window follows it down: 29 KB, good for 0.9 Mbit/s over that round trip, while
//!   4 Mbit/s were being delivered. Again a stall, in the middle of the scroll.
//!
//! So [`Floor`] keeps BBR's window from falling below its initial one, the 240 KB a new
//! connection may send before it knows anything about the path. Above that BBR decides, as
//! for a keyframe on a fast link; below it, flow control does.

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

use quinn_proto::congestion::{BbrConfig, Controller, ControllerFactory, ControllerMetrics};
use quinn_proto::RttEstimator;

/// Builds BBR controllers whose window has a floor.
#[derive(Default)]
pub struct FlooredBbr(Arc<BbrConfig>);

impl ControllerFactory for FlooredBbr {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Floor {
            inner: Arc::clone(&self.0).build(now, current_mtu),
            dead: false,
        })
    }
}

/// A congestion controller whose window is at least its initial one.
struct Floor {
    inner: Box<dyn Controller>,
    /// Everything sent for a while was lost (persistent congestion): no floor until an
    /// acknowledgement shows there is a path again.
    dead: bool,
}

impl Controller for Floor {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        self.inner.on_sent(now, bytes, last_packet_number);
    }

    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
        self.dead = false;
        self.inner.on_ack(now, sent, bytes, app_limited, rtt);
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
    ) {
        self.dead = false;
        self.inner
            .on_end_acks(now, in_flight, app_limited, largest_packet_num_acked);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        lost_bytes: u64,
    ) {
        self.dead |= is_persistent_congestion;
        self.inner
            .on_congestion_event(now, sent, is_persistent_congestion, lost_bytes);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.inner.on_mtu_update(new_mtu);
    }

    fn window(&self) -> u64 {
        let window = self.inner.window();
        if self.dead {
            window
        } else {
            window.max(self.inner.initial_window())
        }
    }

    fn metrics(&self) -> ControllerMetrics {
        self.inner.metrics()
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Floor {
            inner: self.inner.clone_box(),
            dead: self.dead,
        })
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INITIAL: u64 = 240_000;

    /// A controller whose window is whatever the test says: collapsed after a loss while
    /// idle, say.
    #[derive(Clone)]
    struct Fixed(u64);

    impl Controller for Fixed {
        fn on_congestion_event(&mut self, _: Instant, _: Instant, _: bool, _: u64) {}
        fn on_mtu_update(&mut self, _: u16) {}
        fn window(&self) -> u64 {
            self.0
        }
        fn clone_box(&self) -> Box<dyn Controller> {
            Box::new(self.clone())
        }
        fn initial_window(&self) -> u64 {
            INITIAL
        }
        fn into_any(self: Box<Self>) -> Box<dyn Any> {
            self
        }
    }

    fn floor(window: u64) -> Floor {
        Floor {
            inner: Box::new(Fixed(window)),
            dead: false,
        }
    }

    #[test]
    fn the_window_never_falls_below_the_initial_one() {
        // Four packets, after a loss while idle; a stale bandwidth estimate's 29 KB.
        assert_eq!(floor(5808).window(), INITIAL);
        assert_eq!(floor(28_794).window(), INITIAL);
        // Above it the controller decides.
        assert_eq!(floor(800_000).window(), 800_000);
        assert_eq!(floor(5808).initial_window(), INITIAL);
        assert_eq!(floor(5808).clone_box().window(), INITIAL);
    }

    #[test]
    fn a_dead_path_gets_no_floor_until_it_answers() {
        let now = Instant::now();
        let mut f = floor(5808);
        // Losses, however many, are no reason: a lossy path is what the floor is for.
        f.on_congestion_event(now, now, false, 50_000);
        assert_eq!(f.window(), INITIAL);
        // Everything lost for a while: the controller's own window.
        f.on_congestion_event(now, now, true, 50_000);
        assert_eq!(f.window(), 5808);
        assert_eq!(f.clone_box().window(), 5808);
        f.on_congestion_event(now, now, false, 1200);
        assert_eq!(f.window(), 5808);
        // An acknowledgement: there is a path again.
        f.on_end_acks(now, 1200, false, Some(7));
        assert_eq!(f.window(), INITIAL);
    }

    #[test]
    fn the_factory_builds_a_floored_bbr() {
        let now = Instant::now();
        let mut c = Arc::new(FlooredBbr::default()).build(now, 1200);
        let initial = c.initial_window();
        assert_eq!(c.window(), initial);
        // A loss while nothing is in flight: BBR alone would be down to four packets.
        c.on_sent(now, 1200, 1);
        c.on_congestion_event(now, now, false, 1200);
        c.on_end_acks(now, 0, true, None);
        assert_eq!(c.window(), initial);
        assert!(c.into_any().downcast::<Floor>().is_ok());
    }
}
