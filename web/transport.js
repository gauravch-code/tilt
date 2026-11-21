// The stream: tilt's protocol over WebTransport (QUIC, UDP), behind the small socket interface
// app.js and the tests use: readyState, send(), close(), and open, message and close events (as
// on* handlers or listeners). Wire format in docs/protocol.md.
//
// It finds the server's UDP host and port, certificate and ticket at GET transport, next to the
// page.
//
// Where the server turns it on (a long path), the datagram lane runs next to the streams: VIDEO
// arrives as datagrams with parity and is rebuilt from any `k` of them, and ACKs and input go up
// as datagrams as well as on the control stream. A stream waits a round trip for a lost packet;
// the lane does not.
// Anything that keeps the session from coming up ends it with a close event (1006), and the
// page reconnects as after any lost connection.

const VIDEO = 0x01;
const VIDEO_HEADER = 18;
const SHARD = 0x05;
const SHARD_HEADER = 10;
const MAX_RECOVERY_FRAME = 128 * 1024;
// Frames the server may have in flight at once (src/flow.rs), which bounds what waits here.
const MAX_IN_FLIGHT = 256;
// The datagram lane (docs/protocol.md).
const FEC = 0x06;
const FEC_HEADER = 11;
const ACK = 0x10;
const PING = 0x11;
const HAVE = 0x12;
const INPUT = 0x13;
const INPUT_FIRST = 0x20;
const INPUT_LAST = 0x33;
// An INPUT datagram repeats the newest input messages: at most this many, this old and this
// many bytes. Whatever a lost datagram carried, the next one carries again.
const INPUT_RUN = 16;
const INPUT_RUN_MS = 500;
const INPUT_RUN_BYTES = 900;
// The last datagram of a burst has no successor to repair it: it is sent again after these.
const INPUT_REPEAT_MS = [15, 45];
const HAVE_REPEAT_MS = 25;
const KIND_TEXT = 0;
const KIND_BINARY = 1;
const KIND_CLOSE = 2;
const ADVERT_TIMEOUT_MS = 2000;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

const nextSeq = (s) => (s >= 0xffffffff ? 1 : s + 1);
// Distance from a to b going forward through the u32 seq space (which skips 0).
const ahead = (a, b) => (b - a + 0x100000000) % 0x100000000;

// GF(2^8), polynomial 0x11d: the field of the lane's Reed-Solomon code (src/fec.rs).
const GF_EXP = new Uint8Array(512);
const GF_LOG = new Uint8Array(256);
for (let i = 0, x = 1; i < 255; i++) {
  GF_EXP[i] = GF_EXP[i + 255] = x;
  GF_LOG[x] = i;
  x <<= 1;
  if (x & 0x100) x ^= 0x11d;
}
const gfMul = (a, b) => (a && b ? GF_EXP[GF_LOG[a] + GF_LOG[b]] : 0);
const gfInv = (a) => GF_EXP[255 - GF_LOG[a]];
/** The weight of data shard `i` in parity shard `j`. */
const fecWeight = (j, i) => gfMul(255 - j, gfInv((255 - j) ^ i));

/** dst ^= c * src, byte by byte. */
function gfAddMul(dst, src, c) {
  if (!c) return;
  const lc = GF_LOG[c];
  for (let b = 0; b < dst.length; b++) {
    const v = src[b];
    if (v) dst[b] ^= GF_EXP[lc + GF_LOG[v]];
  }
}

function gfScale(row, c) {
  for (let b = 0; b < row.length; b++) row[b] = gfMul(row[b], c);
}

/**
 * The `len`-byte message whose `k` data shards of `size` bytes these are, `parts` mapping shard
 * index to bytes with at least `k` entries; null if they do not determine it. A data shard that
 * is missing is solved from the parity shards: each, less the data that did arrive, is the
 * missing shards under that parity's weights.
 */
export function fecRebuild(len, k, size, parts) {
  const missing = [];
  for (let i = 0; i < k; i++) if (!parts.has(i)) missing.push(i);
  const m = missing.length;
  if (m) {
    const parity = [...parts.keys()].filter((i) => i >= k).slice(0, m);
    if (parity.length < m) return null;
    const rows = parity.map((p) => {
      const bytes = parts.get(p).slice();
      for (const [i, d] of parts) if (i < k) gfAddMul(bytes, d, fecWeight(p - k, i));
      return { w: Uint8Array.from(missing, (i) => fecWeight(p - k, i)), bytes };
    });
    for (let col = 0; col < m; col++) {
      let pivot = col;
      while (pivot < m && !rows[pivot].w[col]) pivot++;
      if (pivot === m) return null;
      [rows[col], rows[pivot]] = [rows[pivot], rows[col]];
      const scale = gfInv(rows[col].w[col]);
      gfScale(rows[col].w, scale);
      gfScale(rows[col].bytes, scale);
      for (let r = 0; r < m; r++) {
        const f = rows[r].w[col];
        if (r === col || !f) continue;
        gfAddMul(rows[r].w, rows[col].w, f);
        gfAddMul(rows[r].bytes, rows[col].bytes, f);
      }
    }
    missing.forEach((i, n) => parts.set(i, rows[n].bytes));
  }
  const out = new Uint8Array(len);
  for (let i = 0; i < k; i++) out.set(parts.get(i).subarray(0, Math.min(size, len - i * size)), i * size);
  return out;
}

function hexBytes(hex) {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(2 * i, 2 * i + 2), 16);
  return out;
}

function hostForUrl(host) {
  // Chrome dials `localhost` over QUIC on ::1 only, which fails where IPv6 is off (some
  // microVMs) or tilt listens on IPv4; 127.0.0.1 reaches both a 0.0.0.0 and a [::] listener.
  if (host === 'localhost') return '127.0.0.1';
  return host.includes(':') && !host.startsWith('[') ? `[${host}]` : host;
}

export class WtSocket extends EventTarget {
  /** `pageUrl` resolves `transport`, so a path prefix in front of tilt is kept. */
  constructor(pageUrl) {
    super();
    this.readyState = 0;
    this.onopen = this.onmessage = this.onclose = this.onskip = null;
    this.wt = null;
    this.writer = null;
    // Video waits until the first control message (welcome) is out: the page acks only then.
    this.heard = false;
    this.expect = 1; // the next VIDEO seq to deliver
    this.early = new Map(); // seq -> { buf, receivedAt }, before ordered delivery
    this.shards = new Map(); // at most the server's frames in flight
    this.recovered = 0;
    this.protected = 0;
    this.lost = 0; // video streams that failed before giving their seq
    // The datagram lane. `datagrams` says the browser has them (for the hello); the lane is
    // used once the server's welcome turns it on (`useLane`).
    this.datagrams = false;
    this.dgram = null; // the datagram writer
    this.lane = false;
    this.groups = new Map(); // seq -> { len, k, size, parts }, frames arriving as datagrams
    this.fecFrames = 0; // frames that arrived whole as datagrams
    this.fecRepaired = 0; // of those, the ones that needed their parity
    this.sent = 0; // ordered messages sent on the control stream: all but ACK and PING
    this.run = []; // the newest input messages, { n, bytes, at }, numbered consecutively
    this.inputTimer = 0;
    this.haveTimer = 0;
    this.start(pageUrl);
  }

  async start(pageUrl) {
    let advert = null;
    try {
      const r = await fetch(new URL('transport', pageUrl), {
        cache: 'no-store',
        signal: AbortSignal.timeout(ADVERT_TIMEOUT_MS),
      });
      if (r.ok) advert = (await r.json()).wt ?? null;
    } catch { /* unreachable, or not JSON */ }
    if (this.readyState === 3) return;
    if (!advert || !advert.port || !Array.isArray(advert.hashes)) {
      this.ended(1006);
      return;
    }
    // A sandbox's public UDP listener can be on another host than the page (--udp-public).
    const host = hostForUrl(typeof advert.host === 'string' && advert.host ? advert.host : new URL(pageUrl).hostname);
    const url = `https://${host}:${advert.port}/stream?ticket=${encodeURIComponent(advert.ticket || '')}`;
    try {
      this.wt = new WebTransport(url, {
        serverCertificateHashes: advert.hashes.map((h) => ({ algorithm: 'sha-256', value: hexBytes(h) })),
        congestionControl: 'low-latency',
      });
      this.wt.closed.then(() => this.ended(1006), () => this.ended(1006));
      await this.wt.ready;
      const bidi = await this.wt.createBidirectionalStream();
      if (this.readyState === 3) return;
      this.writer = bidi.writable.getWriter();
      this.readyState = 1;
      this.openDatagrams();
      this.readControl(bidi.readable);
      this.readVideo();
      this.emit(new Event('open'));
    } catch {
      this.ended(1006);
    }
  }

  send(data) {
    if (this.readyState !== 1) return;
    let kind = KIND_BINARY;
    let bytes;
    if (typeof data === 'string') {
      kind = KIND_TEXT;
      bytes = encoder.encode(data);
    } else if (data instanceof ArrayBuffer) {
      bytes = new Uint8Array(data);
    } else {
      bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    }
    const out = new Uint8Array(5 + bytes.length);
    new DataView(out.buffer).setUint32(0, bytes.length + 1, true);
    out[4] = kind;
    out.set(bytes, 5);
    this.writer.write(out).catch(() => this.ended(1006));
    const type = kind === KIND_BINARY && bytes.length ? bytes[0] : -1;
    if (type === ACK) {
      // Cumulative, so the newest stands for all: whichever copy arrives first counts.
      if (this.lane) this.sendDatagram(bytes);
    } else if (type !== PING) {
      this.ordered(type, bytes);
    }
  }

  /** The browser's datagrams, if it has them: read from now on, written once the lane is on. */
  openDatagrams() {
    try {
      const d = this.wt.datagrams;
      if (!d || !d.readable) return;
      // The default keeps one datagram and drops the rest while the page is busy.
      try { d.incomingHighWaterMark = 4096; } catch { /* fixed in this browser */ }
      try { d.outgoingHighWaterMark = 256; } catch { /* fixed in this browser */ }
      const writable = typeof d.createWritable === 'function' ? d.createWritable() : d.writable;
      this.dgram = writable.getWriter();
      this.datagrams = true;
      this.readDatagrams(d.readable.getReader());
    } catch {
      this.dgram = null;
      this.datagrams = false;
    }
  }

  /** The server's welcome turned the lane on: datagrams go out from here on. */
  useLane() {
    if (this.dgram) this.lane = true;
  }

  sendDatagram(bytes) {
    if (this.readyState !== 1 || !this.dgram) return;
    // A datagram that cannot go is one lost: the control stream carries everything as well.
    this.dgram.write(bytes).catch(() => {});
  }

  /**
   * Message number `sent` on the control stream (the server counts the same way). An input
   * message joins the run that INPUT datagrams repeat; anything else can only go on the
   * stream, so the run starts over after it.
   */
  ordered(type, bytes) {
    this.sent++;
    if (!this.lane) return;
    if (type < INPUT_FIRST || type > INPUT_LAST || bytes.length > 255) {
      this.run = [];
      return;
    }
    this.run.push({ n: this.sent, bytes: bytes.slice(), at: performance.now() });
    this.sendInput();
    clearTimeout(this.inputTimer);
    let repeat = 0;
    const again = () => {
      this.sendInput();
      repeat++;
      if (repeat < INPUT_REPEAT_MS.length) this.inputTimer = setTimeout(again, INPUT_REPEAT_MS[repeat] - INPUT_REPEAT_MS[repeat - 1]);
    };
    this.inputTimer = setTimeout(again, INPUT_REPEAT_MS[0]);
  }

  /** One INPUT datagram: `u8 0x13 | u32 first | { u8 len | message }…`, the run so far. */
  sendInput() {
    const now = performance.now();
    let from = Math.max(0, this.run.length - INPUT_RUN);
    while (from < this.run.length - 1 && now - this.run[from].at > INPUT_RUN_MS) from++;
    let size = 0;
    for (let i = this.run.length - 1; i >= from; i--) {
      size += 1 + this.run[i].bytes.length;
      if (size > INPUT_RUN_BYTES) {
        from = i + 1;
        break;
      }
    }
    this.run = this.run.slice(from);
    if (!this.run.length) return;
    const out = new Uint8Array(5 + this.run.reduce((n, m) => n + 1 + m.bytes.length, 0));
    out[0] = INPUT;
    new DataView(out.buffer).setUint32(1, this.run[0].n, true);
    let at = 5;
    for (const m of this.run) {
      out[at] = m.bytes.length;
      out.set(m.bytes, at + 1);
      at += 1 + m.bytes.length;
    }
    this.sendDatagram(out);
  }

  async readDatagrams(reader) {
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done || this.readyState === 3) return;
        if (value && value.length && value[0] === FEC) this.readFec(value);
      }
    } catch { /* the session went away */ }
  }

  /** One shard of a VIDEO sent as datagrams; any `k` of a frame's rebuild it. */
  readFec(d) {
    if (d.length <= FEC_HEADER) return;
    const dv = new DataView(d.buffer, d.byteOffset, d.byteLength);
    const seq = dv.getUint32(1, true);
    const len = dv.getUint32(5, true);
    const k = d[9];
    const index = d[10];
    const size = d.length - FEC_HEADER;
    // A shard that makes no sense is dropped like a lost one: the server sends a frame that
    // does not arrive this way again on a stream.
    if (!seq || !k || len < VIDEO_HEADER || size !== Math.ceil(len / k)) return;
    if (ahead(this.expect, seq) >= 0x80000000 || this.early.has(seq)) return;
    let group = this.groups.get(seq);
    if (!group) {
      if (this.groups.size >= MAX_IN_FLIGHT) this.groups.delete(this.groups.keys().next().value);
      group = { len, k, size, parts: new Map() };
      this.groups.set(seq, group);
    }
    if (group.len !== len || group.k !== k || group.parts.has(index)) return;
    group.parts.set(index, d.subarray(FEC_HEADER));
    if (group.parts.size < k) return;
    this.groups.delete(seq);
    let repaired = false;
    for (let i = 0; i < k && !repaired; i++) repaired = !group.parts.has(i);
    const video = fecRebuild(len, k, size, group.parts);
    if (!video || video[0] !== VIDEO || new DataView(video.buffer).getUint32(2, true) !== seq) return;
    this.fecFrames++;
    if (repaired) this.fecRepaired++;
    this.acceptVideo(video.buffer);
    this.drainVideo();
  }

  /**
   * Tells the server which frames are here in full: `u8 0x12 | u32 base | u64 bits`, every seq
   * before `base` and `base + 1 + i` for each set bit. What it does not hear of in time it
   * sends again on a stream. Each report is complete, so a lost one is made good by the next;
   * the last is sent twice.
   */
  sendHave(repeat = false) {
    if (!this.lane) return;
    let base = this.expect;
    while (this.early.has(base)) base = nextSeq(base);
    const out = new Uint8Array(13);
    const dv = new DataView(out.buffer);
    out[0] = HAVE;
    dv.setUint32(1, base, true);
    let seq = base;
    for (let i = 0; i < 64; i++) {
      seq = nextSeq(seq);
      if (this.early.has(seq)) dv.setUint32(i < 32 ? 5 : 9, (dv.getUint32(i < 32 ? 5 : 9, true) | (1 << (i % 32))) >>> 0, true);
    }
    this.sendDatagram(out);
    clearTimeout(this.haveTimer);
    if (!repeat) this.haveTimer = setTimeout(() => this.sendHave(true), HAVE_REPEAT_MS);
  }

  /** Like WebSocket.close(): a close event follows, unless the session had already ended. */
  close() {
    if (this.readyState === 3) return;
    this.shut();
    setTimeout(() => this.emit(new CloseEvent('close', { code: 1005 })));
  }

  shut() {
    this.readyState = 3;
    this.early.clear();
    this.shards.clear();
    this.groups.clear();
    this.run = [];
    clearTimeout(this.inputTimer);
    clearTimeout(this.haveTimer);
    if (this.wt) {
      try { this.wt.close(); } catch { /* closed already */ }
    }
  }

  /** The session ended (or never came up): one close event. */
  ended(code) {
    if (this.readyState === 3) return;
    this.shut();
    this.emit(new CloseEvent('close', { code }));
  }

  emit(event) {
    const handler = this[`on${event.type}`];
    if (handler) handler.call(this, event);
    this.dispatchEvent(event);
  }

  deliver(data, receivedAt = performance.now()) {
    if (this.readyState === 3) return;
    this.emit(Object.assign(new MessageEvent('message', { data }), { receivedAt }));
  }

  async readControl(readable) {
    const reader = readable.getReader();
    let buf = new Uint8Array(0);
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        const joined = new Uint8Array(buf.length + value.length);
        joined.set(buf);
        joined.set(value, buf.length);
        buf = joined;
        let at = 0;
        while (buf.length - at >= 4) {
          const len = new DataView(buf.buffer, buf.byteOffset + at, 4).getUint32(0, true);
          if (len < 1 || buf.length - at < 4 + len) break;
          this.onFrame(buf[at + 4], buf.slice(at + 5, at + 4 + len));
          if (this.readyState === 3) return;
          at += 4 + len;
        }
        buf = buf.slice(at);
      }
    } catch { /* the session went away */ }
    this.ended(1006);
  }

  onFrame(kind, payload) {
    if (kind === KIND_CLOSE) {
      const code = payload.length >= 2 ? payload[0] | (payload[1] << 8) : 1005;
      this.heard = true;
      this.ended(code);
      return;
    }
    if (kind === KIND_TEXT) this.deliver(decoder.decode(payload));
    else if (kind === KIND_BINARY) this.deliver(payload.buffer);
    else return;
    if (!this.heard) {
      this.heard = true;
      this.drainVideo();
    }
  }

  async readVideo() {
    const streams = this.wt.incomingUnidirectionalStreams.getReader();
    try {
      for (;;) {
        const { value, done } = await streams.read();
        if (done) return;
        this.readFrame(value);
      }
    } catch { /* the session went away */ }
  }

  async readFrame(stream) {
    let buf;
    let kind;
    try {
      buf = await new Response(stream.pipeThrough(new TransformStream({
        transform(chunk, controller) {
          if (kind === undefined && chunk.length) kind = chunk[0];
          controller.enqueue(chunk);
        },
      }))).arrayBuffer();
    } catch {
      // A reset shard is not a missing VIDEO seq: other shards may already have repaired
      // it. Reconnect rather than skipping an unrelated future frame or waiting forever.
      if (kind === SHARD || this.protected) {
        this.ended(1006);
        return;
      }
      buf = null;
    }
    if (this.readyState === 3) return;
    if (buf && new Uint8Array(buf)[0] === SHARD) {
      this.readShard(buf);
      return;
    }
    if (!buf || buf.byteLength < VIDEO_HEADER || new Uint8Array(buf, 0, 1)[0] !== VIDEO) {
      this.lost++;
    } else {
      this.acceptVideo(buf);
    }
    this.drainVideo();
  }

  acceptVideo(buf) {
    const seq = new DataView(buf).getUint32(2, true);
    if (ahead(this.expect, seq) >= 0x80000000 || this.early.has(seq)) return;
    this.early.set(seq, { buf, receivedAt: performance.now() });
    this.shards.delete(seq);
    this.groups.delete(seq);
  }

  /** Any four of five shards reconstruct a VIDEO, without waiting for retransmission. */
  readShard(buf) {
    if (buf.byteLength < SHARD_HEADER) return this.ended(1006);
    const d = new DataView(buf);
    const seq = d.getUint32(1, true);
    const len = d.getUint32(5, true);
    const index = d.getUint8(9);
    const size = Math.ceil(len / 4);
    if (!seq || len < VIDEO_HEADER || len > MAX_RECOVERY_FRAME || index > 4 ||
        buf.byteLength !== SHARD_HEADER + size) return this.ended(1006);
    if (ahead(this.expect, seq) >= 0x80000000 || this.early.has(seq)) return;
    let group = this.shards.get(seq);
    if (!group) {
      // Do not silently drop a frame at the limit: that would leave cumulative ACKs stuck.
      if (this.shards.size >= MAX_IN_FLIGHT) return this.ended(1006);
      group = { len, parts: new Map() };
      this.shards.set(seq, group);
      this.protected++;
    }
    if (group.len !== len) return this.ended(1006);
    if (group.parts.has(index)) return;
    group.parts.set(index, new Uint8Array(buf, SHARD_HEADER));
    if (group.parts.size < 4) return;
    const parts = group.parts;
    let repaired = false;
    for (let i = 0; i < 4; i++) {
      if (parts.has(i)) continue;
      const missing = new Uint8Array(parts.get(4));
      for (const [j, bytes] of parts) {
        if (j === 4) continue;
        for (let k = 0; k < size; k++) missing[k] ^= bytes[k];
      }
      parts.set(i, missing);
      repaired = true;
    }
    const video = new Uint8Array(len);
    for (let i = 0; i < 4; i++) video.set(parts.get(i).subarray(0, Math.min(size, len - i * size)), i * size);
    // The reconstructed header must agree with its envelope before it can advance ACKs.
    if (video[0] !== VIDEO || new DataView(video.buffer).getUint32(2, true) !== seq) return this.ended(1006);
    if (repaired) this.recovered++;
    this.acceptVideo(video.buffer);
    this.drainVideo();
  }

  /** Delivers VIDEO in seq order. A frame that never came is skipped (and acked by the page). */
  drainVideo() {
    this.sendHave();
    if (!this.heard) return;
    for (;;) {
      const frame = this.early.get(this.expect);
      if (frame) {
        this.early.delete(this.expect);
        this.expect = nextSeq(this.expect);
        this.deliver(frame.buf, frame.receivedAt);
        if (this.readyState === 3) return;
        continue;
      }
      if (!this.lost || !this.early.size) return;
      // A stream failed before its seq was known: assume it held the gap up to the oldest
      // frame waiting, skip that, and let the page ack it and ask for a keyframe.
      this.lost--;
      let oldest = null;
      for (const s of this.early.keys()) {
        if (oldest === null || ahead(this.expect, s) < ahead(this.expect, oldest)) oldest = s;
      }
      const last = oldest === 1 ? 0xffffffff : oldest - 1;
      this.expect = oldest;
      for (const waiting of [this.shards, this.groups]) {
        for (const seq of waiting.keys()) {
          if (ahead(this.expect, seq) >= 0x80000000) waiting.delete(seq);
        }
      }
      if (this.onskip) this.onskip(last);
    }
  }
}
