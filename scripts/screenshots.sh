#!/usr/bin/env bash
# Regenerate the README screenshots in docs/ from the synthetic fixtures (no real server data).
#
#   scripts/screenshots.sh
#
# Renders frames with `cargo run --example peek` (FORMAT=html) and photographs them with headless
# Chrome. Set CHROME= to the browser binary if it isn't found.
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

# shot NAME COLS ROWS [KEYS] [CLIENT]
shot() {
  local name=$1 cols=$2 rows=$3 keys=${4:-} client=${5:-}
  TICKS=60 NAME=nfs-server COLS=$cols ROWS=$rows KEYS=$keys FORMAT=html \
    "$PEEK" tests/fixtures $client > "$TMP/$name.html"
  # Cell size of 13px Menlo at line-height 1.22, plus the page padding (see peek.rs).
  local w h
  w=$(awk -v c="$cols" 'BEGIN { printf "%d", c * 7.83 + 34 }')
  h=$(awk -v r="$rows" 'BEGIN { printf "%d", r * 15.86 + 30 }')
  "$CHROME" --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=2 \
    --window-size="$w,$h" --screenshot="$PWD/docs/$name.png" "file://$TMP/$name.html" >/dev/null 2>&1
  echo "docs/$name.png"
}

shot top 160 21
shot client 140 36 e 10.20.1.11
shot inflight 140 30 i
shot server 140 34 o
