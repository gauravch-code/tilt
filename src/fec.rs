//! Forward error correction for VIDEO over QUIC datagrams (docs/protocol.md, "The datagram
//! lane").
//!
//! A stream resends a lost packet about one round trip later, and since frames decode in
//! order, the picture waits that long: a quarter of a second between continents, for every
//! lost packet. Here a frame instead goes out as `k` data datagrams plus parity datagrams,
//! each its own packet, and any `k` of them rebuild it at once. A frame the viewer still
//! cannot rebuild is sent again on a reliable stream (src/transport.rs), so nothing is ever
//! lost for good: the code only has to make that rare.
//!
//! The code is systematic Reed-Solomon over GF(2^8) (polynomial 0x11d) with a Cauchy matrix
//! whose first column is scaled to ones, so a one-datagram frame's parity is a plain copy.

use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};
use wtransport::quinn::PathStats;

/// Server -> client datagram: one shard of a VIDEO message.
pub const MSG_FEC: u8 = 0x06;
/// `u8 0x06 | u32 seq | u32 len | u8 k | u8 index`, then the shard.
pub const HEADER: usize = 11;
/// Data shards per frame at most; with the parity that is under the code's 256.
pub const MAX_DATA: usize = 192;
/// Parity shards per frame at most.
const MAX_PARITY: usize = 48;
/// At least one parity shard for every this many data shards (rounded up): 25% more bytes,
/// like the stream recovery it replaces.
const DATA_PER_PARITY: usize = 4;
/// More parity than that is added until a frame fails to arrive whole less often than this,
/// at the path's loss rate: one frame in ten thousand is a stall every few minutes of motion.
/// A fixed quarter leaves a frame of four packets one spare, which at 1% loss fails once in a
/// thousand: several stalls a minute.
const FAILURE_TARGET: f64 = 1e-4;
/// The loss rate assumed on a path that has shown none lately, and the highest planned for.
const LOSS_FLOOR: f64 = 0.005;
const LOSS_CEILING: f64 = 0.1;
/// Loss is measured over at least this long and this many packets, and planned for with this
/// much margin: it comes in bursts, and a second's sample misses most of them.
const LOSS_SAMPLE: Duration = Duration::from_secs(1);
const LOSS_SAMPLE_PACKETS: u64 = 50;
const LOSS_MARGIN: f64 = 1.5;
/// A measured rate fades by this much a second while the path loses less.
const LOSS_DECAY: f64 = 0.9;

/// Paths at least this long use the lane, for every frame: below it a resend comes soon
/// enough. (Not only for small frames, or only once the path has lost something, as first
/// tried: QUIC sends datagrams ahead of stream data, so with some frames on each, an older
/// frame's stream waited behind newer frames' datagrams whenever QUIC held packets back,
/// which after an idle spell it does; and the first packet lost stalled the stream.)
const MIN_RTT: Duration = Duration::from_millis(60);
/// More than one frame in this many lost beyond its parity and sent again (at least two, of
/// at least FAILING_FRAMES, counted over a second or two) means the parity is not coping:
/// what the path loses comes in runs, as from
/// a full queue, and sending whole frames again only adds to it. Frames then go on streams,
/// which resend just the lost packets, for FAILING_HOLD, and twice as long each time
/// it happens again, up to FAILING_HOLD_MAX; a quiet FAILING_FORGET starts over.
const FAILING_ONE_IN: u32 = 10;
const FAILING_FRAMES: u32 = 8;
const FAILING_HOLD: Duration = Duration::from_secs(5);
const FAILING_HOLD_MAX: Duration = Duration::from_secs(60);
const FAILING_FORGET: Duration = Duration::from_secs(30);

const fn tables() -> ([u8; 512], [u8; 256]) {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        exp[i + 255] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11d;
        }
        i += 1;
    }
    (exp, log)
}

const TABLES: ([u8; 512], [u8; 256]) = tables();

fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    TABLES.0[usize::from(TABLES.1[usize::from(a)]) + usize::from(TABLES.1[usize::from(b)])]
}

fn inv(a: u8) -> u8 {
    TABLES.0[255 - usize::from(TABLES.1[usize::from(a)])]
}

/// The weight of data shard `i` in parity shard `j`: `x / (x ^ i)` with `x = 255 - j`. Any
/// square part of this matrix is invertible (Cauchy, rows scaled), which is what lets any `k`
/// shards rebuild the frame, as long as `i < 255 - j`.
pub fn coefficient(j: usize, i: usize) -> u8 {
    let x = (255 - j) as u8;
    mul(x, inv(x ^ i as u8))
}

/// How many data and parity shards a `len`-byte VIDEO message takes in datagrams of at most
/// `max` bytes on a path that loses the fraction `loss` of its packets; None when it is too
/// large for the lane.
pub fn layout(len: usize, max: usize, loss: f64) -> Option<(usize, usize)> {
    let room = max.checked_sub(HEADER).filter(|&r| r > 0)?;
    let k = len.div_ceil(room).max(1);
    (k <= MAX_DATA).then(|| (k, parity(k, loss)))
}

/// The parity shards for `k` data shards: a quarter as many, and more while more than that
/// would be lost too often (FAILURE_TARGET) with each packet lost independently at `loss`,
/// up to twice the quarter (or three shards, for the smallest frames, whose parity is cheap):
/// half as much again as the video is the most the lane spends on it.
fn parity(k: usize, loss: f64) -> usize {
    let quarter = k.div_ceil(DATA_PER_PARITY);
    let most = (2 * quarter).clamp(3, MAX_PARITY);
    let mut r = quarter.min(most);
    while r < most && more_lost_than(k + r, r, loss) > FAILURE_TARGET {
        r += 1;
    }
    r
}

/// The probability that more than `r` of `n` packets are lost.
fn more_lost_than(n: usize, r: usize, loss: f64) -> f64 {
    // P(exactly i lost), from i = 0 up.
    let mut term = (1.0 - loss).powi(n as i32);
    let mut within = term;
    for i in 0..r {
        term *= (n - i) as f64 / (i + 1) as f64 * loss / (1.0 - loss);
        within += term;
    }
    (1.0 - within).max(0.0)
}

/// The datagrams of one VIDEO message: `k` data shards, then `r` of parity. The shards share
/// one size, `ceil(len / k)`, the last data shard zero-padded: more than half a packet each
/// when `k` came from [`layout`], so that no two travel in one packet.
pub fn datagrams(video: &[u8], k: usize, r: usize) -> Vec<Bytes> {
    let size = video.len().div_ceil(k);
    let header = |index: usize| {
        let mut d = BytesMut::with_capacity(HEADER + size);
        d.put_u8(MSG_FEC);
        d.put_slice(&video[2..6]);
        d.put_u32_le(video.len() as u32);
        d.put_u8(k as u8);
        d.put_u8(index as u8);
        d
    };
    let mut out = Vec::with_capacity(k + r);
    for (i, chunk) in video.chunks(size).enumerate() {
        let mut d = header(i);
        d.put_slice(chunk);
        d.put_bytes(0, size - chunk.len());
        out.push(d.freeze());
    }
    for j in 0..r {
        let mut parity = vec![0u8; size];
        for (i, chunk) in video.chunks(size).enumerate() {
            let c = coefficient(j, i);
            if c == 1 {
                parity.iter_mut().zip(chunk).for_each(|(p, &b)| *p ^= b);
            } else {
                let row = &TABLES.0[usize::from(TABLES.1[usize::from(c)])..];
                for (p, &b) in parity.iter_mut().zip(chunk) {
                    if b != 0 {
                        *p ^= row[usize::from(TABLES.1[usize::from(b)])];
                    }
                }
            }
        }
        let mut d = header(k + j);
        d.put_slice(&parity);
        out.push(d.freeze());
    }
    out
}

/// Decides, frame by frame, whether the lane carries it, and with how much parity. Reads
/// QUIC's packet statistics for the path's loss, and takes the word of the session's flow
/// control (src/flow.rs) for whether the link is full. An earlier version also stopped at any
/// rise of QUIC's round trip, first of 15 ms and then of 40: jitter alone tripped the first,
/// and either turned parity off just as its own bytes filled a link of middling speed, after
/// which every lost packet stalled the stream and the stalls read as more congestion. The
/// bitrate controller already makes room for the parity, which it sees as a slower link.
pub struct Gate {
    /// The session's flow control finds the link starved (FlowControl::congested): streams
    /// only. A link that drops packets because it is full is not mended by parity, and what
    /// it drops would read as a loss rate calling for more of it.
    pub congested: bool,
    /// The path's loss rate, as last measured and faded since.
    loss: f64,
    /// The rate the last frame was planned for.
    planned: f64,
    /// When that was measured, and the packet counts then: (at, sent, lost).
    sample: Option<(Instant, u64, u64)>,
    /// Frames the lane carried lately and those of them sent again, halved every second
    /// (since when).
    carried: u32,
    resent: u32,
    halved: Option<Instant>,
    /// Too many were sent again: streams only until then, for `hold` the next time.
    failing_until: Option<Instant>,
    hold: Duration,
    was_failing: bool,
}

impl Gate {
    pub fn new() -> Gate {
        Gate {
            congested: false,
            loss: 0.0,
            planned: LOSS_FLOOR,
            sample: None,
            carried: 0,
            resent: 0,
            halved: None,
            failing_until: None,
            hold: FAILING_HOLD,
            was_failing: false,
        }
    }

    /// The lane carried a frame.
    pub fn carried(&mut self, now: Instant) {
        match self.halved {
            Some(at) if now.saturating_duration_since(at) < Duration::from_secs(1) => {}
            _ => {
                self.carried /= 2;
                self.resent /= 2;
                self.halved = Some(now);
            }
        }
        self.carried += 1;
    }

    /// A frame the lane carried was lost beyond its parity (later frames arrived, it did
    /// not) and was sent again.
    pub fn resent(&mut self, now: Instant) {
        self.resent += 1;
        if self.resent < 2
            || self.carried < FAILING_FRAMES
            || self.resent * FAILING_ONE_IN <= self.carried
        {
            return;
        }
        if self
            .failing_until
            .is_some_and(|t| now.saturating_duration_since(t) > FAILING_FORGET)
        {
            self.hold = FAILING_HOLD;
        }
        self.failing_until = Some(now + self.hold);
        self.hold = (self.hold * 2).min(FAILING_HOLD_MAX);
        (self.carried, self.resent) = (0, 0);
    }

    /// The shards for sending a `len`-byte VIDEO message again, at the loss rate last
    /// planned for, whatever the path is doing: a resend is rare and already late.
    pub fn layout(&self, len: usize, max_datagram: usize) -> Option<(usize, usize)> {
        layout(len, max_datagram, self.planned).filter(|_| !self.congested)
    }

    /// The loss rate to plan parity for.
    fn planned_loss(&mut self, path: &PathStats, now: Instant) -> f64 {
        match self.sample {
            None => self.sample = Some((now, path.sent_packets, path.lost_packets)),
            Some((at, sent, lost)) => {
                let elapsed = now.saturating_duration_since(at);
                let packets = path.sent_packets.saturating_sub(sent);
                if elapsed >= LOSS_SAMPLE && packets >= LOSS_SAMPLE_PACKETS {
                    let rate = path.lost_packets.saturating_sub(lost) as f64 / packets as f64;
                    let faded = self.loss * LOSS_DECAY.powf(elapsed.as_secs_f64());
                    // What a full link drops says nothing about the loss parity is for.
                    self.loss = if self.congested {
                        faded
                    } else {
                        rate.max(faded)
                    };
                    self.sample = Some((now, path.sent_packets, path.lost_packets));
                }
            }
        }
        self.planned = (self.loss * LOSS_MARGIN).clamp(LOSS_FLOOR, LOSS_CEILING);
        self.planned
    }

    /// The shards for a `len`-byte VIDEO message, or None when it should go on a stream: a
    /// short path, a frame too large, or a link that is starved or on which too many frames
    /// need sending again.
    pub fn protect(
        &mut self,
        len: usize,
        max_datagram: usize,
        path: &PathStats,
        now: Instant,
    ) -> Option<(usize, usize)> {
        let loss = self.planned_loss(path, now);
        if path.min_rtt < MIN_RTT {
            return None;
        }
        let (k, r) = layout(len, max_datagram, loss)?;
        let failing = self.failing_until.is_some_and(|t| now < t);
        if failing != self.was_failing {
            tracing::debug!(failing, hold = ?self.hold, "lane");
            self.was_failing = failing;
        }
        if self.congested || failing {
            // A full link drops packets in runs, a small frame's copies with it, and then the
            // frame waits longer for the lane's resend than it would for QUIC's: measured at
            // 3 Mbit/s, even a few small frames on the lane cost 200 ms at the 95th percentile.
            return None;
        }
        Some((k, r))
    }
}

/// Rebuilds a VIDEO message from any `k` of its shards, given as (index, shard without the
/// header). The reference decoder for the tests; the web client's is in web/transport.js.
#[cfg(test)]
pub fn rebuild(len: usize, k: usize, shards: &[(usize, Vec<u8>)]) -> Option<Vec<u8>> {
    let size = len.div_ceil(k);
    let mut data: Vec<Option<Vec<u8>>> = vec![None; k];
    let mut parity = Vec::new();
    for (index, shard) in shards {
        if shard.len() != size {
            return None;
        }
        match data.get_mut(*index) {
            Some(slot) => *slot = Some(shard.clone()),
            None => parity.push((*index - k, shard.clone())),
        }
    }
    let missing: Vec<usize> = (0..k).filter(|&i| data[i].is_none()).collect();
    if parity.len() < missing.len() {
        return None;
    }
    parity.truncate(missing.len());
    // Take the data that did arrive out of each parity shard: what is left is the missing
    // shards under that parity's weights, a square system solved by elimination.
    let mut rows: Vec<(Vec<u8>, Vec<u8>)> = parity
        .into_iter()
        .map(|(j, mut p)| {
            for (i, d) in data.iter().enumerate() {
                if let Some(d) = d {
                    let c = coefficient(j, i);
                    p.iter_mut().zip(d).for_each(|(p, &b)| *p ^= mul(c, b));
                }
            }
            (missing.iter().map(|&i| coefficient(j, i)).collect(), p)
        })
        .collect();
    for col in 0..missing.len() {
        let pivot = (col..rows.len()).find(|&r| rows[r].0[col] != 0)?;
        rows.swap(col, pivot);
        let scale = inv(rows[col].0[col]);
        rows[col].0.iter_mut().for_each(|v| *v = mul(*v, scale));
        rows[col].1.iter_mut().for_each(|v| *v = mul(*v, scale));
        let (weights, bytes) = rows[col].clone();
        for (r, row) in rows.iter_mut().enumerate() {
            let factor = row.0[col];
            if r == col || factor == 0 {
                continue;
            }
            row.0
                .iter_mut()
                .zip(&weights)
                .for_each(|(v, &w)| *v ^= mul(factor, w));
            row.1
                .iter_mut()
                .zip(&bytes)
                .for_each(|(v, &b)| *v ^= mul(factor, b));
        }
    }
    for (&i, (_, bytes)) in missing.iter().zip(rows) {
        data[i] = Some(bytes);
    }
    let mut video: Vec<u8> = data.into_iter().flatten().flatten().collect();
    video.truncate(len);
    Some(video)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(len: usize) -> Vec<u8> {
        let mut v: Vec<u8> = (0..len).map(|i| ((i * 37 + i / 251) % 256) as u8).collect();
        v[0] = crate::protocol::MSG_VIDEO;
        v[2..6].copy_from_slice(&77u32.to_le_bytes());
        v
    }

    fn parts(datagrams: &[Bytes]) -> Vec<(usize, Vec<u8>)> {
        datagrams
            .iter()
            .map(|d| (usize::from(d[10]), d[HEADER..].to_vec()))
            .collect()
    }

    #[test]
    fn the_field_is_a_field() {
        for a in 1..=255u8 {
            assert_eq!(mul(a, inv(a)), 1, "{a}");
            assert_eq!(mul(a, 1), a);
        }
        assert_eq!(mul(0x80, 2), 0x1d, "reduced by 0x11d");
        assert_eq!((mul(0, 7), mul(7, 0)), (0, 0));
        // A parity's weight on the first data shard is one: a one-shard frame's parity is a copy.
        for j in 0..MAX_PARITY {
            assert_eq!(coefficient(j, 0), 1);
        }
    }

    #[test]
    fn shards_carry_the_frame_and_its_header() {
        let v = video(2500);
        let (k, r) = layout(v.len(), 1011, 0.0).unwrap();
        assert_eq!((k, r), (3, 1));
        let d = datagrams(&v, k, r);
        assert_eq!(d.len(), 4);
        for (i, shard) in d.iter().enumerate() {
            assert_eq!(shard.len(), HEADER + 834, "equal sizes, over half a packet");
            assert_eq!(shard[0], MSG_FEC);
            assert_eq!(u32::from_le_bytes(shard[1..5].try_into().unwrap()), 77);
            assert_eq!(u32::from_le_bytes(shard[5..9].try_into().unwrap()), 2500);
            assert_eq!((shard[9], shard[10]), (3, i as u8));
        }
        // A frame of one shard: its parity is the same bytes.
        let v = video(300);
        let d = datagrams(&v, 1, 1);
        assert_eq!(d[0][HEADER..], v[..]);
        assert_eq!(d[0][HEADER..], d[1][HEADER..]);
    }

    /// The same bytes as in e2e/mock/tests/transport.spec.mjs, which rebuilds the message
    /// from them with the web client's decoder: both ends hold to one wire format.
    #[test]
    fn parity_matches_the_reference_vector() {
        let mut v: Vec<u8> = (0..40u32).map(|i| (i * 7 + 3) as u8).collect();
        v[0] = crate::protocol::MSG_VIDEO;
        v[2..6].copy_from_slice(&5u32.to_le_bytes());
        let hex = |b: &[u8]| b.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let d = datagrams(&v, 3, 2);
        assert_eq!(hex(&d[3][..HEADER]), "0605000000280000000303");
        assert_eq!(hex(&d[3][HEADER..]), "808a67634243f04b8eff330e7e99");
        assert_eq!(hex(&d[4][HEADER..]), "b9a95c42dac1dea7c5a465576a50");
    }

    #[test]
    fn layouts_fit_the_datagram_and_the_code() {
        assert_eq!(layout(1, 1200, 0.0), Some((1, 1)));
        assert_eq!(layout(1189, 1200, 0.0), Some((1, 1)));
        assert_eq!(layout(1190, 1200, 0.0), Some((2, 1)));
        assert_eq!(layout(5 * 1189, 1200, 0.0), Some((5, 2)));
        assert_eq!(
            layout(MAX_DATA * 1189, 1200, 0.0),
            Some((MAX_DATA, MAX_PARITY))
        );
        assert_eq!(
            layout(MAX_DATA * 1189, 1200, 0.1),
            Some((MAX_DATA, MAX_PARITY))
        );
        assert_eq!(layout(MAX_DATA * 1189 + 1, 1200, 0.0), None);
        assert_eq!(layout(100, HEADER, 0.0), None);
        // Every weight the largest layout uses exists: no shard index meets its parity's x.
        const { assert!(MAX_DATA + MAX_PARITY <= 255) };
    }

    #[test]
    fn parity_grows_with_the_loss_rate() {
        // Two of two packets are both lost with p squared; three of five with 10 p cubed.
        assert!((more_lost_than(2, 1, 0.01) - 1e-4).abs() < 1e-9);
        assert!((more_lost_than(5, 2, 0.01) - 9.85e-6).abs() < 1e-7);
        assert_eq!(more_lost_than(4, 4, 0.5), 0.0);
        let at = |loss: f64| -> Vec<usize> {
            [1, 2, 4, 8, 20, 100]
                .iter()
                .map(|&k| parity(k, loss))
                .collect()
        };
        // Up to half a percent, a quarter is enough but for a frame of four packets, which
        // with one spare fails once in four thousand.
        assert_eq!(at(0.0), [1, 1, 1, 2, 5, 25]);
        assert_eq!(at(0.005), [1, 1, 2, 2, 5, 25]);
        assert_eq!(at(0.012), [2, 2, 2, 3, 5, 25]);
        assert_eq!(at(0.03), [2, 3, 3, 4, 5, 25]);
        // Never more than twice the quarter, or three for the smallest frames.
        assert_eq!(at(0.1), [3, 3, 3, 4, 10, 27]);
        assert_eq!(at(0.5), [3, 3, 3, 4, 10, 48]);
        for loss in [0.005, 0.01, 0.03] {
            for k in [1, 3, 10, 60] {
                let r = parity(k, loss);
                assert!(
                    more_lost_than(k + r, r, loss) <= FAILURE_TARGET,
                    "{k} at {loss}"
                );
            }
        }
    }

    #[test]
    fn any_k_shards_rebuild_the_frame() {
        for (len, max, loss) in [
            (300, 1200, 0.0),
            (2500, 1011, 0.03),
            (9000, 1200, 0.0),
            (40_001, 1200, 0.1),
        ] {
            let v = video(len);
            let (k, r) = layout(len, max, loss).unwrap();
            let all = parts(&datagrams(&v, k, r));
            // Lose every run of `r` shards in turn, wrapping around.
            for start in 0..k + r {
                let kept: Vec<_> = (0..k + r)
                    .filter(|i| (i + k + r - start) % (k + r) >= r)
                    .map(|i| all[i].clone())
                    .collect();
                assert_eq!(kept.len(), k);
                assert_eq!(
                    rebuild(len, k, &kept).as_deref(),
                    Some(&v[..]),
                    "{len} from {start}"
                );
            }
            // One more lost is one too many.
            assert_eq!(rebuild(len, k, &all[..k - 1]), None);
        }
    }

    fn path(rtt_ms: u64, min_ms: u64, lost: u64) -> PathStats {
        let mut p = PathStats::default();
        p.rtt = Duration::from_millis(rtt_ms);
        p.min_rtt = Duration::from_millis(min_ms);
        p.lost_packets = lost;
        p
    }

    #[test]
    fn the_gate_measures_the_loss_rate() {
        let t0 = Instant::now();
        let sec = Duration::from_secs(1);
        let mut g = Gate::new();
        let mut p = path(250, 250, 0);
        assert_eq!(g.planned_loss(&p, t0), LOSS_FLOOR);
        // 3% of a second's packets lost: planned for with the margin.
        (p.sent_packets, p.lost_packets) = (1000, 30);
        assert!((g.planned_loss(&p, t0 + sec) - 0.045).abs() < 1e-9);
        // Too few packets to tell: as before.
        p.sent_packets += 10;
        assert!((g.planned_loss(&p, t0 + 2 * sec) - 0.045).abs() < 1e-9);
        // A clean stretch: the rate fades, down to the floor.
        p.sent_packets += 5000;
        let faded = g.planned_loss(&p, t0 + 3 * sec);
        assert!((faded - 0.045 * LOSS_DECAY * LOSS_DECAY).abs() < 1e-9);
        p.sent_packets += 5000;
        assert_eq!(g.planned_loss(&p, t0 + 60 * sec), LOSS_FLOOR);
        // An outage is not planned for packet by packet.
        p.sent_packets += 100;
        p.lost_packets += 90;
        assert_eq!(g.planned_loss(&p, t0 + 61 * sec), LOSS_CEILING);
    }

    #[test]
    fn the_gate_follows_the_path() {
        let t0 = Instant::now();
        let mut g = Gate::new();
        // A short path resends soon enough.
        assert_eq!(g.protect(300, 1200, &path(20, 20, 0), t0), None);
        // A long one: every frame, whether or not the path has lost anything yet.
        assert_eq!(g.protect(300, 1200, &path(250, 250, 0), t0), Some((1, 1)));
        assert_eq!(g.protect(2000, 1200, &path(250, 250, 0), t0), Some((2, 1)));
        assert_eq!(
            g.protect(20_000, 1200, &path(250, 250, 0), t0),
            Some((17, 5))
        );
        let later = t0 + Duration::from_secs(700);
        assert!(g.protect(20_000, 1200, &path(250, 250, 3), later).is_some());
        // A round trip above the path's lowest, jitter or a queue, changes nothing here: the
        // bitrate controller is what answers a queue.
        assert!(g.protect(20_000, 1200, &path(330, 250, 4), later).is_some());
        // Flow control finds the link starved: streams only, and what the link drops
        // meanwhile does not count as its loss rate.
        g.congested = true;
        assert_eq!(g.protect(20_000, 1200, &path(250, 250, 5), later), None);
        assert_eq!(g.protect(300, 1200, &path(250, 250, 5), later), None);
        assert_eq!(g.layout(20_000, 1200), None);
        assert_eq!(g.layout(300, 1200), None);
        g.congested = false;
        assert!(g.layout(20_000, 1200).is_some());
        // One frame in twenty sent again is loss beyond the parity now and then; more than
        // one in ten is a path the parity does not suit.
        let p = path(250, 250, 6);
        for i in 0..40 {
            g.carried(later);
            if i % 20 == 0 {
                g.resent(later);
            }
        }
        assert!(g.protect(20_000, 1200, &p, later).is_some());
        for _ in 0..3 {
            g.resent(later);
        }
        assert_eq!(g.protect(20_000, 1200, &p, later), None);
        assert_eq!(g.protect(300, 1200, &p, later), None);
        let held = later + FAILING_HOLD;
        assert!(g.protect(20_000, 1200, &p, held).is_some());
        // Again right away: twice as long.
        for _ in 0..20 {
            g.carried(held);
        }
        for _ in 0..3 {
            g.resent(held);
        }
        assert_eq!(g.protect(20_000, 1200, &p, held + FAILING_HOLD), None);
        assert!(g
            .protect(20_000, 1200, &path(250, 250, 7), held + 2 * FAILING_HOLD)
            .is_some());
        // Too large for the code: a stream.
        assert_eq!(g.protect(400_000, 1200, &path(250, 250, 6), later), None);
    }
}
