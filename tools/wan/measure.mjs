// How smoothly continuous motion plays through tilt: a Chrome on the X display scrolls
// source.html one step per display refresh while painting a 12-bit frame counter in its
// corner; a headless Chrome views tilt and reads the counter off the canvas on every refresh.
//
//   node measure.mjs [URL] [SECONDS] [STEP_PX] [HZ]   (DISPLAY is the screen tilt serves)
//
// Prints one JSON line. Per viewer refresh during the motion, the counter should advance by
// exactly one: `repeatPct` is the share of refreshes that showed no new frame (a stutter),
// `skips` the source frames never shown, `gap*` the times between new frames (16.7 ms at
// 60 Hz is perfect), `lat*` source rAF to viewer refresh (includes both browsers' pipelines).
//
// With HZ (say 120), both browsers run with their animation frames freed from the display's
// 60 Hz, which a virtual display cannot raise: the source scrolls HZ times a second, and the
// viewer, which then tells tilt a fast display, looks at its canvas as often as it can.
// `shownFps` is the new frames shown per second; `repeatPct` then means nothing. A headless
// Chrome keeps to 60 Hz regardless, so the viewer then needs a display of its own:
// VIEW_DISPLAY (run.sh starts one).
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(path.join(here, '../../e2e/package.json'));
const { chromium } = require('playwright');
const [url = 'http://127.0.0.1:6090/', secs = '10', step = '12', hz = '0'] = process.argv.slice(2);
const ms = Number(secs) * 1000;
const unpaced = Number(hz) > 0 ? ['--disable-frame-rate-limit', '--disable-gpu-vsync'] : [];

const src = await chromium.launch({
  headless: false,
  args: ['--window-position=0,0', '--window-size=1280,720', '--no-first-run', ...unpaced],
});
const sp = await (await src.newContext({ viewport: null })).newPage();
await sp.goto('file://' + path.join(here, 'source.html'));
await sp.evaluate(([s, h]) => { window.step = s; window.hz = h; }, [Number(step), Number(hz)]);

const view = await chromium.launch(unpaced.length
  ? { headless: false, args: ['--window-size=1280,800', ...unpaced], env: { ...process.env, DISPLAY: process.env.VIEW_DISPLAY || process.env.DISPLAY } }
  : {});
const vp = await (await view.newContext({ viewport: { width: 1280, height: 720 } })).newPage();
// Idle, even an unpaced Chrome runs its animation frames at 60 Hz: say the rate outright.
await vp.goto(Number(hz) > 0 ? `${url}#fps=${hz}` : url);
await vp.waitForFunction(() => window.tilt?.stats.framesDrawn > 0, null, { timeout: 20000 });
await vp.waitForTimeout(2000);
await vp.evaluate(() => {
  const ctx = document.getElementById('screen').getContext('2d');
  // The counter sits below the source browser's toolbar: find its first (black) block.
  const col = ctx.getImageData(16, 0, 1, 300).data;
  let y = 0;
  while (y < 300 && !(col[y * 4] < 40 && col[y * 4 + 1] < 40 && col[y * 4 + 2] < 40)) y++;
  y += 16;
  window.samples = [];
  let last = 0;
  const loop = (now) => {
    // Unpaced animation frames come by the thousand: look every 2 ms at most.
    if (now - last >= 2) {
      last = now;
      const d = ctx.getImageData(0, y, 12 * 32, 1).data;
      let n = 0;
      for (let i = 0; i < 12; i++) if (d[(16 + 32 * i) * 4] > 128) n |= 1 << i;
      window.samples.push([performance.timeOrigin + performance.now(), n]);
    }
    requestAnimationFrame(loop);
  };
  requestAnimationFrame(loop);
});
await sp.evaluate(() => { window.go = true; });
await vp.waitForTimeout(ms);
await sp.evaluate(() => { window.go = false; });
await vp.waitForTimeout(1000);
const samples = await vp.evaluate(() => window.samples);
const shown = await sp.evaluate(() => window.shown);
const smoothMs = await vp.evaluate(() => window.tilt.stats.smoothMs);
const sessionFps = await vp.evaluate(() => fetch('api/status').then((r) => r.json()).then((s) => s.sessions[0]?.fps ?? null, () => null));
// Frames that arrived whole as datagrams, and those of them that needed parity.
const lane = await vp.evaluate(() => ({ fecFrames: window.tilt.ws?.fecFrames ?? 0, fecRepaired: window.tilt.ws?.fecRepaired ?? 0, frames: window.tilt.stats.lastSeq }));
const shownFor = (s) => (s.length > 1 ? (s[s.length - 1][1] - s[0][1]) / 1000 : 1);
await src.close();
await view.close();

const srcTime = new Map(shown);
// The motion: from the first frame shown to the last change (the counter wraps at 4096).
const first = samples.findIndex((s) => s[1] >= 1);
let last = samples.length - 1;
while (last > first && samples[last][1] === samples[last - 1][1]) last--;
const w = samples.slice(first, last + 1);
let repeats = 0;
let skips = 0;
const gaps = [];
const lat = [];
let changedAt = w[0][0];
let wraps = 0;
for (let i = 1; i < w.length; i++) {
  const d = (w[i][1] - w[i - 1][1] + 4096) % 4096;
  if (d === 0) {
    repeats++;
    continue;
  }
  if (d >= 2048) continue; // a misread
  skips += d - 1;
  gaps.push(w[i][0] - changedAt);
  changedAt = w[i][0];
  if (w[i][1] < w[i - 1][1]) wraps++;
  const at = srcTime.get(w[i][1] + 4096 * wraps);
  if (at) lat.push(w[i][0] - at);
}
const pct = (a, p) => {
  const s = [...a].sort((x, y) => x - y);
  return Math.round(s[Math.min(s.length - 1, Math.floor(s.length * p))] * 10) / 10;
};
console.log(JSON.stringify({
  refreshes: w.length - 1,
  sourceFrames: shown.length,
  sourceFps: Math.round((10 * shown.length) / shownFor(shown)) / 10,
  shownFps: Math.round((10 * gaps.length) / ((w[w.length - 1][0] - w[0][0]) / 1000)) / 10,
  repeatPct: Math.round((1000 * repeats) / (w.length - 1)) / 10,
  skips,
  gapP50: pct(gaps, 0.5),
  gapP95: pct(gaps, 0.95),
  gapP99: pct(gaps, 0.99),
  gapMax: pct(gaps, 1),
  latP50: pct(lat, 0.5),
  latP95: pct(lat, 0.95),
  smoothMs,
  serverFps: sessionFps,
  ...lane,
}));
