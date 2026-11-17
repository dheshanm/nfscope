#!/usr/bin/env bash
# Regenerate the README screenshots and demo GIF in docs/ from the synthetic fixtures (no real
# server data).
#
#   scripts/screenshots.sh
#
# Renders frames with `cargo run --example peek` (FORMAT=html) and photographs them with headless
# Chrome; gifski (brew install gifski) assembles docs/demo.gif. Set CHROME= to the browser
# binary if it isn't found.
set -euo pipefail
cd "$(dirname "$0")/.."

CHROME=${CHROME:-}
for c in "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" google-chrome chromium chromium-browser; do
  [ -n "$CHROME" ] && break
  if [ -x "$c" ] || command -v "$c" >/dev/null 2>&1; then CHROME=$c; fi
done
[ -n "$CHROME" ] || { echo "no Chrome/Chromium found; set CHROME=" >&2; exit 1; }

cargo build -q --example peek
PEEK=target/debug/examples/peek
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p docs

# frame OUT.png COLS ROWS TICKS KEYS CLIENT: one rendered frame after TICKS fixture samples.
frame() {
  local out=$1 cols=$2 rows=$3 ticks=$4 keys=$5 client=$6
  TICKS=$ticks NAME=nfs-server COLS=$cols ROWS=$rows KEYS=$keys FORMAT=html \
    "$PEEK" tests/fixtures $client > "$TMP/frame.html"
  # Cell size of 13px Menlo at line-height 1.22, plus the page padding (see peek.rs).
  local w h
  w=$(awk -v c="$cols" 'BEGIN { printf "%d", c * 7.83 + 34 }')
  h=$(awk -v r="$rows" 'BEGIN { printf "%d", r * 15.86 + 30 }')
  "$CHROME" --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=2 \
    --window-size="$w,$h" --screenshot="$out" "file://$TMP/frame.html" >/dev/null 2>&1
}

# shot NAME COLS ROWS [KEYS] [CLIENT]: a still in docs/NAME.png.
shot() {
  frame "$PWD/docs/$1.png" "$2" "$3" 60 "${4:-}" "${5:-}"
  echo "docs/$1.png"
}

shot top 160 21
shot client 140 36 e 10.20.1.11
shot inflight 140 30 i
shot server 140 34 o

# The demo GIF: a short tour, two frames a second, one fixture sample per frame so the graphs
# keep moving. Starts 60 samples in, so the history graphs are already full.
if ! command -v gifski >/dev/null 2>&1; then
  echo "gifski not found (brew install gifski): skipping docs/demo.gif" >&2
  exit 0
fi
mkdir "$TMP/frames"
n=0
# scene FRAMES KEYS CLIENT
scene() {
  local i
  for ((i = 0; i < $1; i++)); do
    frame "$TMP/frames/$(printf %03d $n).png" 140 28 $((60 + n)) "$2" "$3"
    n=$((n + 1))
  done
}
scene 12 "" ""              # live client list
scene 6 sss ""              # sorted by THREADS: who is holding nfsd, not who moves most data
scene 10 e 10.20.1.11       # drill-down: chart, threads by operation, connections, open files
scene 6 i ""                # requests in flight
scene 6 o ""                # nfsd server health
gifski --quiet --fps 2 --width 1280 --quality 90 -o docs/demo.gif "$TMP"/frames/*.png
echo "docs/demo.gif ($n frames)"
