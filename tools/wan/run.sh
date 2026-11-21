#!/usr/bin/env bash
# Measures tilt over an imitated WAN link (docs/performance.md, "Over a lossy link"). Starts
# Xvfb (:99, 1280x720) if needed, the relay, and tilt from target/release, then runs
# measure.mjs (how smoothly a scrolling page plays) or, with MODE=keys, keys.mjs (how long a
# key press takes to show, on tilt-testpattern).
#
#   tools/wan/run.sh "--delay 25 --jitter 4 --mbps 30 --loss 0.01" [SECONDS] [STEP_PX]
#   MODE=keys tools/wan/run.sh "--delay 125 --jitter 5 --loss 0.01" [TRIALS]
#   HZ=120 tools/wan/run.sh "--delay 125 --jitter 5 --loss 0.01"       (see measure.mjs)
#
# Needs Xvfb, python3, curl, node, and Playwright with Chromium (cd e2e && npm ci); MODE=keys
# also target/release/tilt-testpattern (cargo build --release -p tilt-e2e).
# TILT=path picks another binary, TILT_ARGS adds arguments to it.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tilt=${TILT:-$here/../../target/release/tilt}
export DISPLAY=${DISPLAY_WAN:-:99}
pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; }
trap cleanup EXIT

if ! xdpyinfo >/dev/null 2>&1 && ! [ -e "/tmp/.X11-unix/X${DISPLAY#:}" ]; then
  Xvfb "$DISPLAY" -screen 0 1280x720x24 -nolisten tcp >/dev/null 2>&1 &
  pids+=($!)
  sleep 1
fi
if [ -n "${HZ:-}" ]; then
  # The viewer's own display: a headless Chrome draws 60 times a second whatever it is told.
  export VIEW_DISPLAY=${VIEW_DISPLAY:-:98}
  if ! [ -e "/tmp/.X11-unix/X${VIEW_DISPLAY#:}" ]; then
    Xvfb "$VIEW_DISPLAY" -screen 0 1280x800x24 -nolisten tcp >/dev/null 2>&1 &
    pids+=($!)
    sleep 1
  fi
fi
# shellcheck disable=SC2086
python3 "$here/relay.py" 7091 6091 ${1:-} &
pids+=($!)
# shellcheck disable=SC2086
"$tilt" --display "$DISPLAY" --no-auth --bind 127.0.0.1:6090 --udp-port 6091 \
  --udp-public 127.0.0.1:7091 ${TILT_ARGS:-} >"${TILT_OUT:-/dev/null}" 2>&1 &
pids+=($!)
for _ in $(seq 50); do
  curl -sf http://127.0.0.1:6090/healthz >/dev/null && break
  sleep 0.2
done
if [ "${MODE:-scroll}" = keys ]; then
  "$here/../../target/release/tilt-testpattern" --log /dev/null >/dev/null 2>&1 &
  pids+=($!)
  sleep 1
  node "$here/keys.mjs" http://127.0.0.1:6090/ "${2:-100}" 2>/dev/null | grep '^{'
else
  node "$here/measure.mjs" http://127.0.0.1:6090/ "${2:-10}" "${3:-12}" "${HZ:-0}" 2>/dev/null | grep '^{'
fi
