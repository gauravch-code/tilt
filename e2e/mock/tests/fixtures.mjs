// Shared Playwright fixtures: a fresh mock server per test (`mock`, configured with `mockOptions`)
// and small helpers for driving the client and reading the mock's message log.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { test as base, expect } from '@playwright/test';
import { startMockServer } from '../server.mjs';

export { expect };

/**
 * Runs in the page: window.WebTransport carried over the mock's WebSocket (/stream next to the
 * page) in the WebTransport framing (docs/protocol.md): control messages on one bidirectional
 * stream, each VIDEO message on a unidirectional stream of its own. `mode` 'shuffle' delivers
 * every other video stream late, 'lose' fails the 10th, 'never' never connects. 'datagrams' has
 * the datagram lane: VIDEO arrives as datagrams with parity, of which some are lost, and
 * everything the page writes to the control stream arrives 200 ms late, as behind a lost packet.
 */
function installFakeWebTransport({ mode }) {
  const wsUrl = (() => {
    const u = new URL('stream', location.href);
    u.protocol = u.protocol === 'https:' ? 'wss:' : 'ws:';
    u.hash = '';
    return u.href;
  })();
  const log = { created: 0, url: null, hashes: null, lost: 0 };
  window.fakeWt = log;
  const frame = (kind, bytes) => {
    const out = new Uint8Array(5 + bytes.length);
    new DataView(out.buffer).setUint32(0, bytes.length + 1, true);
    out[4] = kind;
    out.set(bytes, 5);
    return out;
  };
  const once = (bytes) => new ReadableStream({ start(c) { c.enqueue(bytes); c.close(); } });
  // The lane's Reed-Solomon code, written from docs/protocol.md: GF(2^8) with polynomial 0x11d.
  const gfMul = (a, b) => {
    let p = 0;
    for (; b; b >>= 1) {
      if (b & 1) p ^= a;
      a <<= 1;
      if (a & 0x100) a ^= 0x11d;
    }
    return p;
  };
  const gfInv = (a) => {
    for (let b = 1; b < 256; b++) if (gfMul(a, b) === 1) return b;
    return 0;
  };
  /** The FEC datagrams of VIDEO message `b`: `k` data shards and `r` of parity. */
  const fecShards = (b, k, r) => {
    const size = Math.ceil(b.length / k);
    return Array.from({ length: k + r }, (_, index) => {
      const d = new Uint8Array(11 + size);
      const dv = new DataView(d.buffer);
      d[0] = 0x06;
      d.set(b.subarray(2, 6), 1);
      dv.setUint32(5, b.length, true);
      d[9] = k;
      d[10] = index;
      if (index < k) {
        d.set(b.subarray(index * size, (index + 1) * size), 11);
      } else {
        const x = 255 - (index - k);
        for (let i = 0; i < k; i++) {
          const w = gfMul(x, gfInv(x ^ i));
          const from = b.subarray(i * size, (i + 1) * size);
          for (let n = 0; n < from.length; n++) d[11 + n] ^= gfMul(w, from[n]);
        }
      }
      return d;
    });
  };
  window.WebTransport = class {
    constructor(url, options) {
      log.created++;
      log.url = url;
      log.hashes = options.serverCertificateHashes.map((h) => [h.algorithm, [...h.value]]);
      let resolveReady;
      let resolveClosed;
      this.ready = new Promise((r) => { resolveReady = r; });
      this.closed = new Promise((r) => { resolveClosed = r; });
      let control;
      let uni;
      let datagrams;
      this.controlReadable = new ReadableStream({ start(c) { control = c; } });
      this.incomingUnidirectionalStreams = new ReadableStream({ start(c) { uni = c; } });
      if (mode === 'datagrams') {
        const wt = this;
        this.datagrams = {
          incomingHighWaterMark: 1,
          outgoingHighWaterMark: 1,
          readable: new ReadableStream({ start(c) { datagrams = c; } }),
          // A datagram goes to the mock as a binary message behind a 0xfd byte.
          writable: new WritableStream({
            write(chunk) {
              log.datagramsOut = (log.datagramsOut || 0) + 1;
              const out = new Uint8Array(1 + chunk.length);
              out[0] = 0xfd;
              out.set(chunk, 1);
              wt.ws.send(out);
            },
          }),
        };
      }
      // As if UDP were blocked: no handshake ever completes.
      if (mode === 'never') return;
      const ws = new WebSocket(wsUrl);
      ws.binaryType = 'arraybuffer';
      this.ws = ws;
      let videos = 0;
      let held = null;
      const flushHeld = () => {
        if (held) uni.enqueue(held);
        held = null;
      };
      ws.onopen = () => resolveReady();
      ws.onmessage = (ev) => {
        if (typeof ev.data === 'string') {
          control.enqueue(frame(0, new TextEncoder().encode(ev.data)));
          return;
        }
        const b = new Uint8Array(ev.data);
        if (b[0] !== 0x01) {
          control.enqueue(frame(1, b));
          return;
        }
        videos++;
        const key = (b[1] & 1) !== 0;
        if (mode === 'datagrams' && videos > 2) {
          const seq = new DataView(b.buffer, b.byteOffset).getUint32(2, true);
          const k = Math.max(1, Math.ceil(b.length / 1000));
          const r = Math.ceil(k / 4) + 1;
          const shards = fecShards(b, k, r);
          if (seq % 10 === 7) {
            // One shard too few ever arrives: the server sends the frame again on a stream.
            shards.slice(0, k - 1).forEach((s) => datagrams.enqueue(s));
            log.resent = (log.resent || 0) + 1;
            setTimeout(() => uni.enqueue(once(b)), 150);
            return;
          }
          // As many lost as there is parity, a different run each frame; one comes twice.
          const lost = seq % (k + r);
          shards.forEach((s, i) => {
            if ((i - lost + k + r) % (k + r) >= r) datagrams.enqueue(s);
          });
          datagrams.enqueue(shards[(lost + r) % (k + r)]);
          return;
        }
        if (mode === 'reorder-delay' && videos === 10) {
          setTimeout(() => uni.enqueue(once(b)), 250);
          return;
        }
        if (mode === 'recovery' && videos % 7 !== 0) {
          const seq = new DataView(b.buffer, b.byteOffset).getUint32(2, true);
          const size = Math.ceil(b.length / 4);
          const parts = Array.from({ length: 5 }, () => new Uint8Array(size + 10));
          parts.forEach((p, i) => {
            const d = new DataView(p.buffer);
            p[0] = 5;
            d.setUint32(1, seq, true);
            d.setUint32(5, b.length, true);
            p[9] = i;
          });
          for (let i = 0; i < b.length; i++) {
            parts[Math.floor(i / size)][10 + i % size] = b[i];
            parts[4][10 + i % size] ^= b[i];
          }
          // Alternate each missing data shard and missing parity; also send duplicates.
          const late = seq % 5;
          parts.forEach((p, i) => {
            if (i !== late && !(seq === 6 && i === 3)) { uni.enqueue(once(p)); uni.enqueue(once(p)); }
          });
          // The first five never receive the missing shard. Frame 6 loses two: recovery
          // must wait for retransmission instead of decoding incomplete bytes.
          if (seq > 5) setTimeout(() => {
            uni.enqueue(once(parts[late]));
            if (seq === 6) uni.enqueue(once(parts[3]));
          }, 300);
          return;
        }
        if (mode === 'lose' && videos === 10 && !key) {
          log.lost++;
          uni.enqueue(new ReadableStream({ start(c) { c.error(new Error('stream reset')); } }));
          return;
        }
        if (mode === 'shuffle') {
          // Every other frame waits for the next one and arrives after it.
          if (held) {
            uni.enqueue(once(b));
            flushHeld();
          } else {
            held = once(b);
            setTimeout(flushHeld, 50);
          }
          return;
        }
        uni.enqueue(once(b));
      };
      ws.onclose = (ev) => {
        try {
          control.enqueue(frame(2, new Uint8Array([ev.code & 0xff, ev.code >> 8])));
          control.close();
        } catch { /* closed */ }
        resolveClosed({ closeCode: ev.code });
      };
    }

    async createBidirectionalStream() {
      const ws = this.ws;
      let welcomed = false;
      const writable = new WritableStream({
        write(chunk) {
          // transport.js writes one whole frame per write.
          const kind = chunk[4];
          const payload = chunk.slice(5);
          const send = () => {
            if (ws.readyState !== 1) return;
            if (kind === 0) ws.send(new TextDecoder().decode(payload));
            else if (kind === 1) ws.send(payload);
          };
          // With the lane, the stream is slow (but for the hello): its datagrams get there first.
          if (mode === 'datagrams' && welcomed) setTimeout(send, 200);
          else send();
          welcomed = true;
        },
      });
      return { readable: this.controlReadable, writable };
    }

    close() {
      if (this.ws) this.ws.close();
    }
  };
}

export const test = base.extend({
  mockOptions: [{}, { option: true }],
  // How the fake WebTransport behaves (see installFakeWebTransport).
  wtMode: ['plain', { option: true }],
  // Every page, in this context or a new one, gets the fake WebTransport before it loads.
  fakeWt: [async ({ context, browser, wtMode }, use) => {
    await context.addInitScript(installFakeWebTransport, { mode: wtMode });
    const newPage = browser.newPage.bind(browser);
    browser.newPage = async (...args) => {
      const p = await newPage(...args);
      await p.addInitScript(installFakeWebTransport, { mode: wtMode });
      return p;
    };
    await use();
    browser.newPage = newPage;
  }, { auto: true }],
  mock: async ({ mockOptions }, use) => {
    const mock = await startMockServer({ quiet: true, ...mockOptions });
    await use(mock);
    await mock.close();
  },
  // Every page in the test's browser contexts: uncaught errors and console output, for the report.
  pageLog: [async ({ context, browser }, use, info) => {
    const lines = [];
    const watch = (page) => {
      page.on('pageerror', (e) => lines.push(`pageerror: ${e.message}`));
      page.on('console', (m) => lines.push(`console.${m.type()}: ${m.text()}`));
    };
    context.on('page', watch);
    const newPage = browser.newPage.bind(browser);
    browser.newPage = async (...args) => {
      const p = await newPage(...args);
      watch(p);
      return p;
    };
    await use(lines);
    browser.newPage = newPage;
    if (lines.length) await info.attach('page-log', { body: lines.join('\n'), contentType: 'text/plain' });
    if (process.env.PAGE_LOG) for (const l of lines) process.stdout.write(`[${info.project.name}] ${info.title}: ${l}\n`);
  }, { auto: true }],
});

// The marker colours of the fixtures (and of tilt-testpattern), in counter order.
export const COLORS = [[230, 40, 40], [40, 200, 60], [40, 80, 230], [240, 240, 240]];
export const BACKGROUND = [32, 32, 32];

export const INPUT_TYPES = ['MOVE', 'BUTTON', 'WHEEL', 'KEY', 'TEXT', 'RELEASE_ALL', 'CLIPBOARD'];

/** The codec string the client should derive from a fixture's first SPS. */
export function fixtureCodec(stream) {
  const data = fs.readFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'fixtures', `${stream}.h264`));
  for (let i = 0; i + 7 < data.length; i++) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1 && (data[i + 3] & 0x1f) === 7) {
      return 'avc1.' + [...data.subarray(i + 4, i + 7)].map((b) => b.toString(16).padStart(2, '0')).join('').toUpperCase();
    }
  }
  throw new Error(`no SPS in ${stream}`);
}

/** Loads the client with URL fragment options and waits for its debug hook. */
export async function open(page, mock, hash = 'token=devtoken') {
  await page.goto(`${mock.url}#${hash}`);
  await page.waitForFunction(() => window.tilt !== undefined);
}

export const stats = (page) => page.evaluate(() => ({ ...window.tilt.stats }));

export async function waitDrawn(page, n, timeout = 10_000) {
  await page.waitForFunction((min) => window.tilt.stats.framesDrawn >= min, n, { timeout });
}

/** Waits until this page holds control and has drawn a frame; returns its session id. */
export async function controlling(page) {
  await page.waitForFunction(() => window.tilt.stats.control && window.tilt.stats.framesDrawn > 0);
  return page.evaluate(() => window.tilt.stats.session);
}

export async function sessionOf(page) {
  await page.waitForFunction(() => window.tilt.stats.session !== '');
  return page.evaluate(() => window.tilt.stats.session);
}

/** Messages of the given types from one session, logged at or after index `since`. */
export function from(mock, session, types, since = 0) {
  const want = new Set(Array.isArray(types) ? types : [types]);
  return mock.messages.slice(since).filter((m) => m.session === session && want.has(m.type));
}

/** Waits until `count` messages of `types` arrived, then a further `settle` ms, and returns them. */
export async function collect(mock, session, types, since, count, settle = 150) {
  await expect.poll(() => from(mock, session, types, since).length, { timeout: 5000 }).toBeGreaterThanOrEqual(count);
  if (settle) await new Promise((r) => setTimeout(r, settle));
  return from(mock, session, types, since);
}

export const keyLog = (msgs) => msgs.map((m) => `${m.down ? 'down' : 'up'} ${m.keysym.toString(16)}`);

/** Input messages in short form: 'down 61', 'TEXT abc', 'BUTTON 1 down', 'WHEEL 0,2', 'RELEASE_ALL'. */
export const inputLog = (msgs) => msgs.map((m) => {
  switch (m.type) {
    case 'KEY': return `${m.down ? 'down' : 'up'} ${m.keysym.toString(16)}`;
    case 'TEXT': return `TEXT ${m.text}`;
    case 'CLIPBOARD': return `CLIPBOARD ${m.text}`;
    case 'BUTTON': return `BUTTON ${m.button} ${m.down ? 'down' : 'up'}`;
    case 'WHEEL': return `WHEEL ${m.dx},${m.dy}`;
    default: return m.type;
  }
});

/**
 * Dispatches synthetic keyboard events on the hidden textarea: [[type, KeyboardEventInit], ...].
 * For layouts and keys Playwright's keyboard cannot produce (macOS Option characters, AZERTY).
 */
export async function keyEvents(page, list) {
  await page.evaluate((events) => {
    const ta = document.getElementById('kbd');
    for (const [type, init] of events) {
      ta.dispatchEvent(new KeyboardEvent(type, { bubbles: true, cancelable: true, composed: true, ...init }));
    }
  }, list);
}

export const isApple = (page) => page.evaluate(() => /Mac|iPhone|iPad|iPod/.test(navigator.platform));

export function expectColor(actual, expected, tolerance = 16) {
  const diff = expected.map((v, i) => Math.abs(v - actual[i]));
  expect(Math.max(...diff), `colour ${actual.slice(0, 3)} vs ${expected}`).toBeLessThanOrEqual(tolerance);
}

export const sample = (page, x, y) => page.evaluate(([px, py]) => window.tilt.sample(px, py), [x, y]);

export async function canvasRect(page) {
  return page.evaluate(() => {
    const r = document.getElementById('screen').getBoundingClientRect();
    return { left: r.left, top: r.top, width: r.width, height: r.height };
  });
}

/** The normalized coordinates the protocol expects for a client point (brief 4.3). */
export function norm(rect, x, y) {
  const c = (v) => Math.min(1, Math.max(0, v));
  return [Math.round(c((x - rect.left) / rect.width) * 65535), Math.round(c((y - rect.top) / rect.height) * 65535)];
}

/** Fakes page visibility (the hidden-tab path) and fires visibilitychange. */
export async function setVisibility(page, state) {
  await page.evaluate((v) => {
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => v });
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => v === 'hidden' });
    document.dispatchEvent(new Event('visibilitychange'));
  }, state);
}

/** The page's state, as posted to a host page in `tilt:state` (docs/guide.md, "Embedding"). */
export const state = (page) => page.evaluate(() => window.tilt.state());

/** Polls one field of the page's state (`status`, `message`, `detail`, `code`, `control`...). */
export const pollState = (page, key, options) => expect.poll(async () => (await state(page))[key], options);

/** Sends a host page command, `{type: 'tilt:…', ...}`, as if the host page had posted it. */
export const command = (page, msg) => page.evaluate((m) => window.tilt.command(m), msg);

export { startMockServer };
