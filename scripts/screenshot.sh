#!/usr/bin/env bash
# Renders apptop's demo data (APPTOP_DEMO=1, made-up programs) in a private tmux server and
# writes docs/screenshot.html; open it in a browser and screenshot the terminal box for
# docs/screenshot.png.
set -euo pipefail
cd "$(dirname "$0")/.."
cols=${COLS:-150} rows=${ROWS:-35}
cargo build --release --quiet
tmux="tmux -L apptop-screenshot -f /dev/null"
$tmux kill-server 2>/dev/null || true
$tmux new-session -d -s s -x "$cols" -y "$rows" -e APPTOP_DEMO=1 -e TERM=xterm-256color \
    "$PWD/target/release/apptop --lang en"
sleep 1.5
# select "Claude Code ×8" (third row) and expand it
$tmux send-keys -t s Down Down Right
sleep 0.5
$tmux capture-pane -t s -e -p | python3 scripts/ansi2html.py "$cols" > docs/screenshot.html
$tmux kill-server
echo "wrote docs/screenshot.html"
