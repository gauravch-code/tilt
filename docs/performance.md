# tilt on CPU-only VMs

Notes for running tilt inside small CPU-only VMs, Firecracker microVMs in particular: what costs
CPU, what helps, what was tried and doesn't, and how to measure an encoder change.

Measured in a Firecracker microVM (4 vCPUs, Intel Xeon with AVX-512, kvm-clock) on a 1080p Xvfb
desktop with Chromium and xterm, tilt pinned to one vCPU and the viewer on another.

## What tilt costs while you work

One viewer at 1080p, in % of one vCPU:

| activity | tilt | before 0.2.0 |
|---|---|---|
| typing in a terminal | 2–3% | 11% |
| scrolling a web page | 23% | 27–30% |
| dragging a window over a page | 35% | 44% |
| nobody watching, or a still screen | ~0% | ~0% |

Encoding one 1080p frame with OpenH264 takes about 3 ms here, and most of that is the same
whatever changed: OpenH264 works through all 8160 macroblocks of the screen, and a still
frame costs as much as a typed character. So small changes skip the encoder (below). Every
viewer has its own encoder.

## Small changes are patches

When what changed since the viewer's last update covers at most an eighth of the screen, and
its pixels, run-length coded, fit in a frame's share of the bitrate (8 KiB at least), tilt
sends those pixels instead of an H.264 frame (`src/video/patch.rs`, docs/protocol.md). A
typed character is a patch of about 300 bytes that takes 6 µs to make, where a frame took
3–5 ms to encode and as long again to decode in the browser. Key press to drawn fell from
12 ms to 3 ms (p50, Chrome, local). Scrolls, window moves and video still go through H.264:
as pixels they would cost far more bytes.

Capture grabs only the damaged rows, and since 0.2.0 it also copies only the rows changed
since the buffer it reuses, rather than the whole last frame (3 MB at 1080p).

## Encoder settings

Background detection and the deblocking filter are off. Both are camera tools; on the
recordings below, turning both off makes frames 10–25% faster to encode and 15–25% smaller, at
1–3 dB more PSNR, at 8 Mbps and at 800 kbps alike. With background detection on, the
refinement of a still screen also stalled 2–3 dB short, since it skipped the macroblocks it
took for background.

## On a small VM

- **Give the VM 2 vCPUs or more.** On one vCPU, tilt's encoder and the desktop's apps take turns,
  and frames wait for the app.
- **Keep AVX2 in the CPU template.** OpenH264 picks its SIMD code (SSE2 to AVX2) at run time.
  Firecracker's T2/C3 templates keep AVX2; a template that hides it makes encoding several times
  slower.
- **`--max-fps 30`** halves encoding CPU while the screen is busy; a still screen costs nothing
  either way.
- **A smaller screen, not a scaled stream.** A 1280×720 Xvfb encodes in about 40–60% of the
  time of 1080p. Scaling a 1080p screen down before encoding does not pay (see below).

## Tried and not worth it

Each measured on recordings of typing, scrolling and dragging (the benchmark below):

| idea | result |
|---|---|
| `--profile baseline` with the settings above | no faster than High any more, for 10–15% more bits |
| mimalloc keeping freed memory for 1 s (`purge_delay`) | no change |
| OpenH264 screen-content mode | 3× the CPU per frame (10–15 ms) for 20–40% fewer bits |
| 2 or 4 encoder threads (one slice each) | at best 10–20% faster frames on idle vCPUs, slower when typing, 5–25% more bits |
| scaling 1080p to 720p or 900p before encoding | only 20–30% faster to encode, but 2–8× the bits: scrolls and window moves stop being whole-pixel motion |
| OpenH264's own downscaler | as slow as encoding 1080p, and 3–4× the bits |
| raising tilt's priority (nice −10, autogroup nice, SCHED_RR) | no change in latency with the vCPU saturated by other processes |

A halted vCPU wakes fast here: about 0.5 ms of key-to-picture latency, so polling to keep vCPUs
awake would not pay either.

## Frame pacing

Capture grabs on its own 60 Hz grid (with an input pull-forward). The encoder used to keep a
second grid, and once the two drifted apart, which an input pull-forward was enough for, every
frame of an animation waited for the encoder's slot: 9.5 ms on average between grab and encode,
on every frame, for as long as the screen kept changing. The encoder now limits the frame rate
with a rate limiter instead (`Schedule::new_slot` in `src/worker.rs`): a new capture is encoded
as soon as it arrives, and over any stretch at most one per frame interval goes out (plus one).
Grab to encode is now about 1 ms, and key-to-picture p95 on an animated screen stays at
17–22 ms where it ranged 18–34 ms. The frame rate cap, and with it the CPU, is unchanged.

## Over a lossy link

On a path that loses packets, as across the internet to a Sail sandbox, QUIC resends the lost
packet about one round trip later. Its frame waits for it, and so does every frame after it,
since the browser decodes them in order, so their ACKs come a round trip late too. Credit used
to cover one round trip of frames, so each loss also stopped the encoder: the picture froze,
then jumped. Credit now covers two round trips while the path holds no queue of ours, and
falls back to one once a queue builds (`frame_cap` in `src/flow.rs`), so a slow link queues no
more than before.

Continuous scrolling in Chrome at 60 fps, 720p, through `tools/wan/run.sh` (a UDP relay with
delay, jitter, loss and a bandwidth cap), share of the viewer's refreshes that showed no new
frame:

| link | before | after |
|---|---|---|
| 50 ms round trip | 3.3% | 0.6% |
| 50 ms round trip, 1% loss | 15% | 8% |
| 100 ms round trip, 0.5% loss | 31% | 17% |
| 50 ms round trip, 3 Mbit/s | ~75% | ~75% |

Latency was unchanged on all four. Those measurements predate optional recovery: on long,
lossy paths the server could then split a frame across four reliable data streams and one XOR
parity stream (it still does for a client without datagrams; for the web client the datagram
lane has taken its place, see [Across continents](#across-continents)). Any four reconstruct it, without waiting for the fifth stream's retransmission.
Recovery adds about 25% payload to protected frames, is negotiated with the client, and backs
off for five seconds when QUIC RTT shows queue growth. See [the wire format](protocol.md#optional-recovery-streams).
The client's ACK processing time also includes the wait behind an earlier frame, so that wait
no longer masquerades as network queue delay.

At 250 ms RTT, 1% independent packet loss in each direction, 30 Mbit/s, no configured jitter,
and 1280×720, three 20-second runs per variant on a 4-vCPU x86-64 orb
gave these medians (same scrolling workload, default smoothing):

| metric | before | ACK timing only | ACK timing + recovery |
|---|---|---|---|
| refreshes without a new frame | 47.0% | 49.2% | 36.4% |
| p95 gap between new frames | 133 ms | 183 ms | 50 ms |
| p95 source-animation-to-viewer latency | 415 ms | 415 ms | 349 ms |

ACK timing alone corrects the congestion signal but does not remove loss stalls. Recovery is
not a cure for constrained bandwidth: at 3 Mbit/s it stayed disabled, and repeated refreshes
ranged 72.6–76.5% before and 70.7–79.2% after. These short, unseeded simulations include startup
transients, not measurements of a real intercontinental route. Input still needs a round trip;
the animation latency above measures only server animation to browser display.

```sh
cargo build --release
tools/wan/run.sh "--delay 25 --jitter 4 --mbps 30 --loss 0.01" 20
```

## Across continents

Between India and the US a packet takes about 125 ms each way, and that round trip is the
least a key press can take to show: tilt adds 7 to 10 ms to it (below), so what is left to win
is not the time itself but what a long, lossy path makes of it. Two things did, before 0.7.

**A lost packet cost a round trip.** A stream resends a lost packet a round trip later;
frames decode in order, so the picture stands still that long, and input behind a lost packet
waits as long on its way up. At 1% loss and 60 frames a second, that is a quarter-second
freeze several times a second. A session on a path of 60 ms or more now has the datagram lane
([protocol](protocol.md#the-datagram-lane)):

- **Video** goes as QUIC datagrams, each frame cut into packets with Reed-Solomon parity
  packets added, a quarter as many and more as the path's measured loss asks. Any lost packets
  up to the number of parity packets are rebuilt on arrival, with nothing resent and nothing
  waited for. A frame that still does not arrive whole is sent again, on a reliable stream,
  as soon as the viewer reports later frames arriving without it.
- **Input** goes as datagrams too, each repeating the last few messages, beside the control
  stream: a lost packet is made good by the next one, 8 ms later rather than 250.
- **ACKs** likewise, so that one held up behind a lost packet does not read as congestion and
  lower the bitrate.

The streams stay the reference: whatever the lane drops arrives on them, late. The lane stands
down, for seconds at a time and longer if it keeps happening, when more than a tenth of the
frames need sending again, or when frames wait for credit with the bitrate at its floor. Both
mean a link that is full, whose drops come in runs that parity does not mend and to which
whole frames sent again are only more load.

**The start of motion waited a round trip.** QUIC's congestion controller (quinn's BBR) sits
under tilt's flow control, and for a desktop it judged badly. After a loss noticed while the
screen was still, it held its window to four packets until something sent later had been
acknowledged, so the first frames of a scroll sat in QUIC's send queue for a round trip: in
the path statistics, a window of 5808 bytes and nothing sent for 250 ms while 130 KB queued,
on about every other start of motion. And it takes no bandwidth sample while the application
sends less than the window allows, which a desktop nearly always does, so that now and then
its window followed a stale estimate down to 29 KB in the middle of a scroll. Its window now
has a floor, the 240 KB a new connection may send (`src/congestion.rs`); below that, flow
control decides, which sees every frame acknowledged.

All through `tools/wan/run.sh` (125 ms each way, up to 5 ms of jitter, 30 Mbit/s unless said),
1280×720 on a 4-vCPU x86-64 box, 0.6.2 against 0.7. Scrolling a page at 60 fps for 15 s, three
runs each (0.6.2: two to five): the share of the viewer's refreshes that showed no new frame,
and the 95th percentile from the source's animation frame to the viewer's (the one-way trip
is 125 to 130 ms; both browsers' own pipelines are in it):

| link | no new frame, before | after | p95 latency, before | after |
|---|---|---|---|---|
| no loss | 0.6–5.4% | 0.4–1.7% | 199 ms | 199 ms |
| 0.5% loss | 17–30% | 0.4–1.7% | 200–282 ms | 199 ms |
| 1% loss | 27–47% | 0.8–1.2% | 266–417 ms | 199 ms |
| 3% loss | 68–69% | 0.7–1.1% | 432–449 ms | 199 ms |
| 1% loss, 15 ms jitter | 47–59% | 0.8–4.8% | 433–435 ms | 199–200 ms |
| 1% loss, 8 Mbit/s | 34–46% | 5.4–6.7% | 300–416 ms | 199 ms |
| 0.5% loss, 3 Mbit/s | 48–69% | 59–75% | 383–549 ms | 533–616 ms |

On the five rows from 0.5% loss to 8 Mbit/s, the longest wait for a new frame fell from
270–720 ms to 50–82 ms: not one freeze in 15 runs. The "no loss" row's 5.4% is one run of five in which 0.6.2 froze for
520 ms as the scroll began. The last row is a link that cannot carry the stream at all (the
scroll wants about 5 Mbit/s at the highest quantizer): there the lane only gets in the way
until it stands down, and over a minute 0.7 showed no new frame on 66% of refreshes against
60%, with 5% fewer frames and 70 ms more at the 95th percentile. On short paths nothing
changes, the lane being off: 50 ms round trip with 1% loss, 5.8–8.0% against 6.4–6.9%.
With the smoother arrival the page's playout delay (`smoothMs`) also falls, from its 45 ms
ceiling to 10 to 20 ms, which is where the median latency gain on lossy links comes from.

A key press to its change drawn (`MODE=keys`, 80 presses a quarter to half a second apart),
median / 95th percentile / slowest, in ms:

| link | before | after |
|---|---|---|
| no loss | 263 / 265 / 266 | 262 / 265 / 266 |
| 1% loss | 262 / 265 / 551 | 261 / 264 / 266 |
| 3% loss | 263 / 588 / 823 | 262 / 266 / 267 |

These are simulations with independent losses; a real route loses in bursts as well, which
parity covers less well. The relay also delivers packets that share a moment in any order,
which QUIC counts as losses: even its "no loss" link shows the controller a few percent.

```sh
cargo build --release --workspace
tools/wan/run.sh "--delay 125 --jitter 5 --mbps 30 --loss 0.01" 15
MODE=keys tools/wan/run.sh "--delay 125 --jitter 5 --mbps 30 --loss 0.01" 80
```

## 120 frames a second

`--max-fps` goes to 120 and is 120 by default; a viewer's page measures its display's refresh
rate and the session is capped at that, so a 60 Hz viewer costs what it did. OpenH264's rate
control knows no frame rate above 60 and gives each frame the bitrate's share for a sixtieth
of a second, so faster sessions tell it a bitrate lowered to match (`rate_control_bps` in
`src/video/encoder.rs`); the level in the SPS stays the one for 60 fps, which decoders do not
hold a stream to. Capture grabs as often as the fastest viewer waiting can take a frame, and
flow control's credit, counted in frames, covers two round trips at the session's rate (it
stopped at 32 frames, one round trip of 250 ms at 120 fps).

With `tilt-testpattern --animate --fps 120` and `tilt-probe --fps 120` on the same box, 720p:
119.97 fps with 8.3 ms between frames (p95 8.6 ms) locally, and 117.6 fps through a 250 ms
round trip. A terminal scrolling 120 lines a second in a third of the screen, to Chrome
through 250 ms and 1% loss: 119.7 frames a second received and 119.8 decoded, none failing,
for 38% of one core against 25% at 60 fps. What a viewer sees of that depends on its display
and on the application drawing that often: most X applications draw 60 times a second.

```sh
HZ=120 tools/wan/run.sh "--delay 125 --jitter 5 --mbps 30 --loss 0.01" 15 6
```

## Measuring an encoder change

Record each workload losslessly, keep one frame per screen change, and convert to raw I420:

```sh
ffmpeg -f x11grab -framerate 60 -video_size 1920x1080 -i :99 -t 10 -c:v ffv1 scroll.mkv
mkdir -p /dev/shm/rec
ffmpeg -i scroll.mkv -vf mpdecimate=hi=1:lo=1:frac=0 -fps_mode passthrough \
  -pix_fmt yuv420p -f rawvideo /dev/shm/rec/scroll.yuv
```

Then run the benchmark (`src/video/bench.rs`) pinned to one vCPU:

```sh
TILT_BENCH_DIR=/dev/shm/rec taskset -c 1 \
  cargo test --release bench_recordings -- --ignored --nocapture
```

It prints encode time per frame (mean, p50, p95, max), bitrate at 60 fps and mean QP for each
`--profile`. Add a variant in `bench.rs` to try a new setting. `TILT_BENCH_OUT=dir` writes each
stream, to compare quality against the source with ffmpeg's `psnr` filter.
