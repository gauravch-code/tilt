//! A session's connection: a WebTransport session (src/wt.rs, docs/protocol.md).
//!
//! One client-opened bidirectional stream (the control stream) carries every message but VIDEO,
//! each framed as `u32 length (LE) | u8 kind | payload`, where `length` counts the kind byte and
//! the payload, and kind is KIND_TEXT (a JSON message), KIND_BINARY (a binary message) or
//! KIND_CLOSE (`u16 code (LE) | utf8 reason`). Each VIDEO access unit goes on a unidirectional
//! stream of its own: its 18-byte header, then the whole access unit, so a lost packet of one
//! frame holds up neither the next frames' transfer nor cursor, PONG or control messages.
//!
//! On a long path a session that asked for it also has the datagram lane (`Lane`): VIDEO as
//! datagrams with parity (src/fec.rs), and the client's ACKs and input as datagrams next to
//! the control stream. A stream waits a round trip for a lost packet; the lane does not.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes, BytesMut};
use wtransport::error::StreamReadError;
use wtransport::{Connection, RecvStream, SendStream, VarInt};

use crate::fec;
use crate::flow::seq_le;
use crate::protocol::MSG_VIDEO;
use crate::recovery::{self, Recovery};

pub const KIND_TEXT: u8 = 0;
pub const KIND_BINARY: u8 = 1;
pub const KIND_CLOSE: u8 = 2;

/// Close code sent in a KIND_CLOSE without one.
const NO_STATUS: u16 = 1005;
/// Bytes read from the control stream at a time.
const READ_CHUNK: usize = 16 * 1024;

/// The second copy of a one-datagram frame goes out this much later: in another packet, and
/// past a short burst of loss.
const COPY_DELAY: Duration = Duration::from_millis(4);
/// A frame sent as datagrams that the viewer has not reported whole this long after two round
/// trips goes out again on a stream. Patient, because it is for the frame with none after it
/// to show the gap (see OVERTAKEN_BY), whose every copy must have been lost; a report that a
/// queue on the path merely delayed must not have the frame sent again into that queue.
const RESEND_MARGIN: Duration = Duration::from_millis(50);
/// Sooner, once the viewer reports a frame this many seqs later whole: datagrams keep their
/// order but for the odd one, so what two later frames overtook is lost.
const OVERTAKEN_BY: u32 = 2;

/// One message of the protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Bytes),
    Close(Option<CloseFrame>),
    /// A datagram from the client (the lane): an ACK, HAVE or INPUT.
    Datagram(Bytes),
}

/// Which VIDEO seqs the viewer has in full, as its HAVE datagrams and ACKs report: shared by
/// the session's reader, which hears them, and its `Sink`, which resends what is missing.
#[derive(Debug, Default)]
pub struct Arrived {
    have: Mutex<Have>,
    /// Told when a report shows a gap: a frame arrived after one that has not.
    gap: tokio::sync::Notify,
}

#[derive(Debug, Default, Clone, Copy)]
struct Have {
    /// Every seq before this one has arrived.
    base: u32,
    /// Bit i: seq `base + 1 + i` has arrived.
    bits: u64,
    /// The newest seq acked: drawn, so it and all before it arrived.
    acked: u32,
}

impl Arrived {
    /// A HAVE datagram. Datagrams can overtake each other: an older report changes nothing.
    pub fn have(&self, base: u32, bits: u64) {
        {
            let mut h = self.lock();
            if base == h.base {
                h.bits |= bits;
            } else if seq_le(h.base, base) {
                h.base = base;
                h.bits = bits;
            }
        }
        if bits != 0 {
            self.gap.notify_one();
        }
    }

    /// Resolves when a report has shown a gap since the last call resolved.
    pub async fn gap(&self) {
        self.gap.notified().await;
    }

    /// The newest seq reported whole; 0 before any.
    fn newest(&self) -> u32 {
        let h = *self.lock();
        let reported = match h.bits {
            0 => h.base.wrapping_sub(1),
            bits => h.base.wrapping_add(64 - bits.leading_zeros()),
        };
        // Before the first report (base 0), only ACKs say anything.
        if h.base != 0 && (h.acked == 0 || seq_le(h.acked, reported)) {
            reported
        } else {
            h.acked
        }
    }

    /// A cumulative ACK.
    pub fn acked(&self, seq: u32) {
        let mut h = self.lock();
        if seq_le(h.acked, seq) {
            h.acked = seq;
        }
    }

    fn has(&self, seq: u32) -> bool {
        let h = *self.lock();
        let ahead = seq.wrapping_sub(h.base);
        (h.acked != 0 && seq_le(seq, h.acked))
            || (ahead != 0 && seq_le(seq, h.base))
            || ((1..=64).contains(&ahead) && h.bits & (1 << (ahead - 1)) != 0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Have> {
        self.have.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The sending half of the datagram lane.
struct Lane {
    gate: fec::Gate,
    arrived: Arc<Arrived>,
    /// Frames sent as datagrams and not yet known to have arrived, oldest first.
    unconfirmed: VecDeque<Unconfirmed>,
    /// Second copies of one-datagram frames, and when to send each.
    copies: VecDeque<(Instant, Bytes)>,
}

struct Unconfirmed {
    seq: u32,
    msg: Bytes,
    priority: i32,
    sent: Instant,
}

/// When a frame sent at `sent` over a path with round trip `rtt` should have been reported.
fn resend_at(sent: Instant, rtt: Duration) -> Instant {
    sent + 2 * rtt + RESEND_MARGIN
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFrame {
    pub code: u16,
    pub reason: String,
}

/// Writes a session's messages.
pub struct Sink {
    conn: Connection,
    control: SendStream,
    /// Each video stream gets a lower priority than the one before, so QUIC sends (and resends)
    /// the oldest frame first, as the decoder needs them, rather than interleaving them.
    video_priority: i32,
    recovery: Option<Recovery>,
    lane: Option<Lane>,
}

impl Sink {
    /// `control` is the session's control stream, which goes ahead of all video.
    pub fn new(conn: Connection, control: SendStream) -> Sink {
        control.set_priority(i32::MAX);
        Sink {
            conn,
            control,
            video_priority: 0,
            recovery: None,
            lane: None,
        }
    }

    pub fn enable_recovery(&mut self) {
        self.recovery = Some(Recovery::new(Instant::now()));
    }

    /// Turns the datagram lane on if the peer takes datagrams; `arrived` is what the session's
    /// reader hears back. False when it does not, and video stays on streams.
    pub fn enable_datagrams(&mut self, arrived: Arc<Arrived>) -> bool {
        if self.conn.max_datagram_size().is_none() {
            return false;
        }
        self.lane = Some(Lane {
            gate: fec::Gate::new(),
            arrived,
            unconfirmed: VecDeque::new(),
            copies: VecDeque::new(),
        });
        true
    }

    /// Whether the session's flow control finds the link congested (FlowControl::congested):
    /// video then keeps to streams.
    pub fn set_congested(&mut self, congested: bool) {
        if let Some(lane) = self.lane.as_mut() {
            lane.gate.congested = congested;
        }
    }

    /// When `on_deadline` next has something to do: a second copy to send, or a frame the
    /// viewer should have reported by then.
    pub fn deadline(&self) -> Option<Instant> {
        let lane = self.lane.as_ref()?;
        let rtt = self.conn.rtt();
        let copy = lane.copies.front().map(|c| c.0);
        let resend = lane.unconfirmed.front().map(|u| resend_at(u.sent, rtt));
        match (copy, resend) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Sends the second copies that are due, and again every frame that went out as
    /// datagrams and did not arrive: one the viewer would have reported whole by now, or one
    /// that later frames have overtaken. Call it at `deadline`, and when `Arrived::gap` says
    /// a report showed a gap.
    pub async fn on_deadline(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        let rtt = self.conn.rtt();
        loop {
            let Some(lane) = self.lane.as_mut() else {
                return Ok(());
            };
            while lane.copies.front().is_some_and(|c| c.0 <= now) {
                let (_, copy) = lane.copies.pop_front().expect("front exists");
                // Best effort: the resend below covers a copy that cannot go.
                let _ = self.conn.send_datagram(&copy);
            }
            let Some(u) = lane.unconfirmed.front() else {
                return Ok(());
            };
            let arrived = lane.arrived.has(u.seq);
            let newest = lane.arrived.newest();
            let overtaken = newest != 0 && seq_le(u.seq.wrapping_add(OVERTAKEN_BY), newest);
            if !arrived && !overtaken && resend_at(u.sent, rtt) > now {
                return Ok(());
            }
            let u = lane.unconfirmed.pop_front().expect("front exists");
            if !arrived {
                tracing::debug!(seq = u.seq, bytes = u.msg.len(), overtaken, "resending");
                // On a stream, which gets it there whatever happens.
                if overtaken {
                    // Later frames arrived and this one did not: it was lost, more of it
                    // than its parity covers.
                    lane.gate.resent(now);
                }
                self.video_stream(&u.msg, u.priority).await?;
                if overtaken {
                    // And as datagrams, which get it there sooner if the stream loses a
                    // packet of its own. Not for a frame that is merely unheard of: it may
                    // be late with everything else (QUIC holding packets back after a loss,
                    // a queue on the path), and then every frame in flight comes due here at
                    // once.
                    self.resend_datagrams(&u.msg, now);
                }
            }
        }
    }

    fn resend_datagrams(&mut self, msg: &Bytes, now: Instant) {
        let (Some(lane), Some(max)) = (self.lane.as_mut(), self.conn.max_datagram_size()) else {
            return;
        };
        // (Not on a congested link, where it would be one more load: `layout` says.)
        if let Some((k, r)) = lane.gate.layout(msg.len(), max) {
            Self::send_shards(&self.conn, lane, msg, k, r, now);
        }
    }

    /// Sends `msg` as `k` data shards and `r` of parity; false if a datagram could not go.
    fn send_shards(
        conn: &Connection,
        lane: &mut Lane,
        msg: &[u8],
        k: usize,
        r: usize,
        now: Instant,
    ) -> bool {
        let mut shards = fec::datagrams(msg, k, r);
        if k == 1 {
            // Its parity is copies, small enough to share the first one's packet: later,
            // each in its own.
            let later = shards.drain(1..).zip(1..);
            lane.copies
                .extend(later.map(|(copy, n)| (now + COPY_DELAY * n, copy)));
            lane.copies.make_contiguous().sort_by_key(|c| c.0);
        }
        shards.iter().all(|shard| conn.send_datagram(shard).is_ok())
    }

    pub async fn send(&mut self, msg: Message) -> anyhow::Result<()> {
        match msg {
            Message::Text(t) => self.frame(KIND_TEXT, t.as_bytes()).await,
            Message::Binary(b) if b.first() == Some(&MSG_VIDEO) => self.video(&b).await,
            Message::Binary(b) => self.frame(KIND_BINARY, &b).await,
            // Datagrams only come in.
            Message::Datagram(_) => Ok(()),
            Message::Close(frame) => {
                let (code, reason) =
                    frame.map_or((NO_STATUS, Default::default()), |f| (f.code, f.reason));
                let mut payload = Vec::with_capacity(2 + reason.len());
                payload.extend_from_slice(&code.to_le_bytes());
                payload.extend_from_slice(reason.as_bytes());
                self.frame(KIND_CLOSE, &payload).await
            }
        }
    }

    async fn frame(&mut self, kind: u8, payload: &[u8]) -> anyhow::Result<()> {
        self.control.write_all(&frame(kind, payload)).await?;
        Ok(())
    }

    /// VIDEO as datagrams with parity where the lane takes it, else an ordinary VIDEO stream
    /// or negotiated recovery shards. Every shard keeps its frame's priority; control stays
    /// ahead of streams. Finishing never waits for the peer's acknowledgement.
    async fn video(&mut self, msg: &Bytes) -> anyhow::Result<()> {
        let priority = self.video_priority;
        self.video_priority = priority.checked_sub(1).unwrap_or(0);
        if self.video_datagrams(msg, priority) {
            return Ok(());
        }
        let protect = self.lane.is_none()
            && self.recovery.as_mut().is_some_and(|r| {
                r.protect(
                    msg.len(),
                    &self.conn.quic_connection().stats().path,
                    Instant::now(),
                )
            });
        if protect {
            for shard in recovery::shards(msg) {
                self.video_stream(&shard, priority).await?;
            }
        } else {
            self.video_stream(msg, priority).await?;
        }
        Ok(())
    }

    /// Sends `msg` on the lane; false when it should go on a stream instead.
    fn video_datagrams(&mut self, msg: &Bytes, priority: i32) -> bool {
        let (Some(lane), Some(max)) = (self.lane.as_mut(), self.conn.max_datagram_size()) else {
            return false;
        };
        let now = Instant::now();
        let path = self.conn.quic_connection().stats().path;
        let Some((k, r)) = lane.gate.protect(msg.len(), max, &path, now) else {
            return false;
        };
        if !Self::send_shards(&self.conn, lane, msg, k, r, now) {
            // The path's packets shrank, or the peer stopped taking datagrams: a stream
            // carries the frame, and what the viewer got twice it drops.
            return false;
        }
        lane.gate.carried(now);
        let seq = u32::from_le_bytes(msg[2..6].try_into().expect("4 bytes"));
        lane.unconfirmed.push_back(Unconfirmed {
            seq,
            msg: msg.clone(),
            priority,
            sent: now,
        });
        true
    }

    async fn video_stream(&mut self, msg: &[u8], priority: i32) -> anyhow::Result<()> {
        let mut stream = self.conn.open_uni().await?.await?;
        stream.set_priority(priority);
        stream.write_all(msg).await?;
        // Errs only on a stream already reset; the data is queued either way.
        let _ = stream.quic_stream_mut().finish();
        Ok(())
    }

    /// Flushes the control stream, waiting until the peer has all of it (a close message
    /// included), then ends the connection: closing it at once would drop unacknowledged data.
    pub async fn close(&mut self) -> anyhow::Result<()> {
        let flushed = self.control.finish().await;
        self.conn.close(VarInt::from_u32(0), b"");
        Ok(flushed?)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        // A session that ends without `close` (a write timeout) still ends its connection.
        self.conn.close(VarInt::from_u32(0), b"");
    }
}

/// Reads a session's messages from its control stream, and its datagrams.
pub struct Stream {
    conn: Connection,
    recv: RecvStream,
    buf: BytesMut,
    max_msg: usize,
    /// Datagrams are read until the connection stops giving any.
    datagrams: bool,
}

impl Stream {
    /// Messages longer than `max_msg` bytes end the session.
    pub fn new(conn: Connection, recv: RecvStream, max_msg: usize) -> Stream {
        Stream {
            conn,
            recv,
            buf: BytesMut::new(),
            max_msg,
            datagrams: true,
        }
    }

    /// The next message; None once the peer has finished. Cancel-safe: a message is never
    /// half-consumed by a dropped call.
    pub async fn next(&mut self) -> Option<anyhow::Result<Message>> {
        loop {
            match parse_frame(&mut self.buf, self.max_msg) {
                Ok(Some(msg)) => return Some(Ok(msg)),
                Ok(None) => {}
                Err(e) => return Some(Err(e)),
            }
            let mut chunk = [0u8; READ_CHUNK];
            // RecvStream::read is cancel-safe: no data is consumed by a call that is dropped.
            // So is receive_datagram, which waits on a channel.
            let read = tokio::select! {
                read = self.recv.read(&mut chunk) => read,
                datagram = self.conn.receive_datagram(), if self.datagrams => match datagram {
                    Ok(d) => return Some(Ok(Message::Datagram(d.payload()))),
                    Err(_) => {
                        // The connection is going; the stream says how.
                        self.datagrams = false;
                        continue;
                    }
                },
            };
            match read {
                Ok(Some(n)) => self.buf.extend_from_slice(&chunk[..n]),
                Ok(None) if self.buf.is_empty() => return None,
                Ok(None) => {
                    return Some(Err(anyhow::anyhow!(
                        "the control stream ended inside a message"
                    )))
                }
                Err(StreamReadError::Reset(_)) if self.buf.is_empty() => return None,
                Err(e) => return Some(Err(e.into())),
            }
        }
    }
}

/// One control-stream frame.
pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len() + 1).expect("messages are far below 4 GiB");
    let mut out = Vec::with_capacity(5 + payload.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    out
}

/// Takes the first whole frame off `buf`; None until one has arrived.
fn parse_frame(buf: &mut BytesMut, max_msg: usize) -> anyhow::Result<Option<Message>> {
    let Some(len) = buf.get(..4) else {
        return Ok(None);
    };
    let len = u32::from_le_bytes(len.try_into().expect("4 bytes")) as usize;
    anyhow::ensure!(
        (1..=max_msg + 1).contains(&len),
        "control-stream message of {len} bytes is outside 1..={}",
        max_msg + 1
    );
    if buf.len() < 4 + len {
        return Ok(None);
    }
    buf.advance(4);
    let kind = buf.get_u8();
    let payload = buf.split_to(len - 1).freeze();
    Ok(Some(match kind {
        KIND_TEXT => Message::Text(String::from_utf8(payload.to_vec())?),
        KIND_BINARY => Message::Binary(payload),
        KIND_CLOSE => Message::Close(close_frame(&payload)?),
        k => anyhow::bail!("unknown control-stream message kind {k}"),
    }))
}

fn close_frame(payload: &Bytes) -> anyhow::Result<Option<CloseFrame>> {
    let Some(code) = payload.get(..2) else {
        return Ok(None);
    };
    let code = u16::from_le_bytes(code.try_into().expect("2 bytes"));
    let reason = std::str::from_utf8(&payload[2..])?;
    Ok(Some(CloseFrame {
        code,
        reason: reason.to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(bytes: &[u8], max: usize) -> Vec<anyhow::Result<Option<Message>>> {
        let mut buf = BytesMut::from(bytes);
        let mut out = Vec::new();
        loop {
            let r = parse_frame(&mut buf, max);
            let stop = !matches!(r, Ok(Some(_)));
            out.push(r);
            if stop {
                return out;
            }
        }
    }

    #[test]
    fn frames_round_trip_and_wait_for_whole_messages() {
        let mut wire = frame(KIND_TEXT, br#"{"t":"hello","v":1}"#);
        wire.extend(frame(KIND_BINARY, &[0x11, 1, 2, 3, 4]));
        wire.extend(frame(KIND_CLOSE, &[0xe9, 0x03, b'o', b'k']));
        // Byte by byte: nothing is returned until a frame is whole, and nothing is lost.
        let mut buf = BytesMut::new();
        let mut got = Vec::new();
        for b in &wire {
            buf.extend_from_slice(&[*b]);
            if let Some(m) = parse_frame(&mut buf, 64).unwrap() {
                got.push(m);
            }
        }
        assert!(buf.is_empty());
        assert_eq!(
            got,
            [
                Message::Text(r#"{"t":"hello","v":1}"#.into()),
                Message::Binary(Bytes::from_static(&[0x11, 1, 2, 3, 4])),
                Message::Close(Some(CloseFrame {
                    code: 1001,
                    reason: "ok".into()
                })),
            ]
        );
    }

    #[test]
    fn arrivals_come_from_reports_and_acks() {
        let a = Arrived::default();
        assert!(!a.has(1) && !a.has(5));
        assert_eq!(a.newest(), 0);
        // Before any report, an ACK is all there is.
        a.acked(3);
        assert!(a.has(1) && a.has(3) && !a.has(4));
        assert_eq!(a.newest(), 3);
        // Everything before 7 arrived, and 9 and 10 after it: 7 and 8 are missing.
        a.have(7, 0b110);
        assert!(a.has(6) && !a.has(7) && !a.has(8) && a.has(9) && a.has(10) && !a.has(11));
        assert_eq!(a.newest(), 10);
        // The same report again adds what arrived since; an older one changes nothing.
        a.have(7, 0b111);
        assert!(a.has(8) && !a.has(7));
        a.have(5, 0);
        assert!(a.has(6) && a.has(9));
        // A later report replaces it.
        a.have(12, 0);
        assert!(a.has(7) && a.has(11) && !a.has(12) && !a.has(13));
        assert_eq!(a.newest(), 11);
        a.have(12, 1 << 63);
        assert!(a.has(76) && !a.has(75) && !a.has(77));
        assert_eq!(a.newest(), 76);
        // An ACK ahead of the reports counts too.
        a.acked(80);
        assert!(a.has(77) && a.has(80) && !a.has(81));
        assert_eq!(a.newest(), 80);
    }

    #[tokio::test]
    async fn a_report_with_a_gap_wakes_the_sender() {
        let a = Arrived::default();
        let woken = |a: &Arrived| {
            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            std::pin::pin!(a.gap()).poll(&mut cx).is_ready()
        };
        use std::future::Future;
        assert!(!woken(&a));
        a.have(4, 0);
        assert!(!woken(&a), "nothing missing");
        a.have(4, 0b10);
        assert!(woken(&a), "6 arrived and 4 did not");
        assert!(!woken(&a), "once");
    }

    #[test]
    fn frames_are_sent_again_two_round_trips_on() {
        let t0 = Instant::now();
        let rtt = Duration::from_millis(250);
        assert_eq!(resend_at(t0, rtt), t0 + Duration::from_millis(550));
    }

    #[test]
    fn bad_frames_are_errors() {
        // Too long, empty, unknown kind, text that is not UTF-8.
        assert!(parse_all(&frame(KIND_BINARY, &[0; 65]), 64)[0].is_err());
        assert!(parse_all(&frame(KIND_BINARY, &[0; 64]), 64)[0].is_ok());
        assert!(parse_all(&[0, 0, 0, 0], 64)[0].is_err());
        assert!(parse_all(&frame(9, b"x"), 64)[0].is_err());
        assert!(parse_all(&frame(KIND_TEXT, &[0xff]), 64)[0].is_err());
        // A close without a code is a close without a frame.
        let r = parse_all(&frame(KIND_CLOSE, &[]), 64);
        assert!(matches!(r[0], Ok(Some(Message::Close(None)))));
    }
}
