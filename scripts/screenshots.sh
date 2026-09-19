#!/usr/bin/env bash
# Render TUI screenshots (PNG) of a recorded session from the TUI's own frame
# buffers: mobius-searcher --snapshot → HTML → headless Chrome → PNG.
# usage: scripts/screenshots.sh SESSION_ID OUT_DIR
set -euo pipefail
SESSION=${1:-latest}
OUT=${2:-docs/screenshots}
BIN=${BIN:-./target/release/mobius-searcher}
CHROME=${CHROME:-"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"}
mkdir -p "$OUT"
shot() { # name WxH keys [extra args...]
  local name=$1 size=$2 keys=$3; shift 3
  "$BIN" --replay "$SESSION" --snapshot "$size" --pages "" --shot-name "$name" --keys "$keys" --out "$OUT" "$@" >/dev/null
}
U=(--glyphs unicode --color truecolor)
for s in 80x24 100x30 120x40 160x50; do shot "overview" "$s" "1" "${U[@]}"; done
shot "markets-line"     120x40 "2" "${U[@]}"
shot "markets-candle"   120x40 "2c" "${U[@]}"
shot "markets-candle-1h" 120x40 "2c]]" "${U[@]}"
shot "graphs-braille"   120x40 "4s" "${U[@]}"
shot "opportunities"    120x40 "3<down*4>" "${U[@]}"
shot "graphs-ab"        160x50 "4a<left*60>b" "${U[@]}"
shot "graphs-ab"        120x40 "4a<left*60>b" "${U[@]}"
shot "trades"           120x40 "5" "${U[@]}"
shot "risk"             120x40 "6" "${U[@]}"
shot "system"           120x40 "7" "${U[@]}"
shot "logs"             120x40 "8" "${U[@]}"
shot "stream-detail"    120x40 "1<tab><tab><tab><up><enter>" "${U[@]}"
shot "picker"           120x40 "4+" "${U[@]}"
shot "help"             120x40 "1?" "${U[@]}"
shot "overview-ascii"   80x24 "1" --glyphs ascii --color ansi256
shot "graphs-ascii"     100x30 "4a<left*40>b" --glyphs ascii --color none
for f in "$OUT"/*.html; do
  size=${f%.html}; size=${size##*-}          # …-WxH
  cols=${size%x*}; rows=${size#*x}
  w=$(( cols * 8 + 36 )); h=$(( rows * 17 + 30 ))
  "$CHROME" --headless=new --disable-gpu --hide-scrollbars --window-size="$w,$h" \
    --screenshot="${f%.html}.png" "file://$(cd "$(dirname "$f")" && pwd)/$(basename "$f")" >/dev/null 2>&1
done
ls "$OUT"/*.png
