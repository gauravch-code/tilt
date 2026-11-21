// How long a key press takes to show through tilt: tilt-testpattern is on the X display and
// changes its marker's colour on every key press; a headless Chrome views tilt, presses a
// key through the page's own input path and times the colour change on its canvas.
//
//   node keys.mjs [URL] [TRIALS]
//
// Prints one JSON line. `p50`..`max` are key press to drawn, in ms: the link's round trip
// plus tilt's own pipeline at best, and a round trip more whenever a packet's loss is waited
// out. `late` counts the presses that took over 1.5 times the median.
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(path.join(here, '../../e2e/package.json'));
const { chromium } = require('playwright');
const [url = 'http://127.0.0.1:6090/', trials = '100'] = process.argv.slice(2);

// tilt-testpattern's marker (tools/e2e/src/lib.rs): its colours in counter order, at 192,192.
const COLORS = [[230, 40, 40], [40, 200, 60], [40, 80, 230], [240, 240, 240]];
const KEYSYM_A = 0x61;
const TIMEOUT_MS = 5000;

const browser = await chromium.launch();
const page = await (await browser.newContext({ viewport: { width: 1280, height: 720 } })).newPage();
await page.goto(`${url}#remote=1&probe=192,192`);
await page.waitForFunction(() => window.tilt?.stats.framesDrawn > 0 && window.tilt.stats.control, null, { timeout: 20000 });
await page.waitForTimeout(2000);

const times = await page.evaluate(async ({ colors, keysym, trials, timeout }) => {
  const colour = (rgb) => colors.findIndex((c) => Math.hypot(rgb[0] - c[0], rgb[1] - c[1], rgb[2] - c[2]) < 60);
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const out = [];
  for (let i = 0; i < trials; i++) {
    const before = colour(window.tilt.sample(192, 192));
    const events = window.tilt.probeEvents;
    const from = events.length;
    const t0 = performance.now();
    window.tilt.command({ type: 'tilt:key', keysym });
    window.tilt.command({ type: 'tilt:key', keysym, down: false });
    let at = null;
    while (at === null && performance.now() - t0 < timeout) {
      await sleep(4);
      for (let e = from; e < events.length; e++) {
        const c = colour(events[e].rgb);
        if (c >= 0 && c !== before) {
          at = events[e].at;
          break;
        }
      }
    }
    out.push(at === null ? null : at - t0);
    // Unevenly spaced, like typing, and long enough for the refinement frames to pass.
    await sleep(250 + Math.random() * 250);
  }
  return out;
}, { colors: COLORS, keysym: KEYSYM_A, trials: Number(trials), timeout: TIMEOUT_MS });
const client = await page.evaluate(() => ({
  datagrams: window.tilt.ws?.lane === true,
  fecFrames: window.tilt.ws?.fecFrames ?? 0,
  fecRepaired: window.tilt.ws?.fecRepaired ?? 0,
}));
await browser.close();

const seen = times.filter((t) => t !== null).sort((a, b) => a - b);
const pct = (p) => (seen.length ? Math.round(seen[Math.min(seen.length - 1, Math.floor(seen.length * p))] * 10) / 10 : null);
const p50 = pct(0.5);
console.log(JSON.stringify({
  trials: times.length,
  missed: times.length - seen.length,
  p50,
  p90: pct(0.9),
  p95: pct(0.95),
  p99: pct(0.99),
  max: pct(1),
  late: seen.filter((t) => t > 1.5 * p50).length,
  ...client,
}));
